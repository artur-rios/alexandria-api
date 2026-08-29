//! Running an enrichment pass.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::auth::AuthService;
use crate::catalog::clock::Clock;
use crate::config::{MetadataSettings, MetadataUnavailable};
use crate::enrichment::model::{
    ArtistImage, EnrichmentOutcome, EnrichmentReport, EnrichmentScope, TrackLyrics,
};
use crate::enrichment::providers::{
    ArtistIdentityProvider, ArtistImageAsset, ArtistImageProvider, LyricsProvider, LyricsQuery,
    MIN_ARTIST_SCORE,
};
use crate::enrichment::repos::{EnrichmentCandidate, EnrichmentRepository};
use crate::errors::DomainError;

/// Where fetched image bytes are put.
///
/// Its own port rather than a method on `catalog::fs::Filesystem`: that trait
/// is text-only by design (`write_file` takes a `&str`) and is implemented by
/// every fake in the catalog's suite, so widening it for one feature would
/// make eight unrelated test doubles grow a method they will never call.
#[allow(async_fn_in_trait)]
pub trait ArtistImageStore: Send + Sync {
    /// Persist `asset` for `mbid` and return the path to it, relative to the
    /// store's own root. Relative, so the stored path survives the cache
    /// directory being moved or the library being opened on another machine.
    async fn store(&self, mbid: &str, asset: &ArtistImageAsset) -> Result<String, DomainError>;
}

/// The image store over a real directory.
pub struct FsArtistImageStore {
    root: PathBuf,
}

impl FsArtistImageStore {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }
}

impl ArtistImageStore for FsArtistImageStore {
    async fn store(&self, mbid: &str, asset: &ArtistImageAsset) -> Result<String, DomainError> {
        // The MBID is a uuid from MusicBrainz, but it arrives over the
        // network, so it is not trusted as a path component: anything that
        // is not a uuid character is refused rather than sanitized, because
        // a sanitized name could still collide with another artist's.
        if mbid.is_empty() || !mbid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(DomainError::InvalidInput(
                "artist id is not a safe file name".to_string(),
            ));
        }

        tokio::fs::create_dir_all(&self.root)
            .await
            .map_err(|e| DomainError::Disk(e.to_string()))?;

        let name = format!("{mbid}.{}", asset.extension);
        tokio::fs::write(self.root.join(&name), &asset.bytes)
            .await
            .map_err(|e| DomainError::Disk(e.to_string()))?;

        Ok(name)
    }
}

/// Run an enrichment pass over a scope (music enrichment design).
///
/// Generic over every collaborator for the reason the rest of this crate is:
/// the decisions worth testing are which items are skipped, what score is
/// accepted, and what outcome is recorded — none of which should need a
/// network, a key, or a service that might be down while the suite runs.
pub struct EnrichHandler<A, R, I, P, L, S, C> {
    auth: A,
    repo: R,
    identity: I,
    images: P,
    lyrics: L,
    store: S,
    clock: C,
    settings: MetadataSettings,
}

impl<A, R, I, P, L, S, C> EnrichHandler<A, R, I, P, L, S, C>
where
    A: AuthService,
    R: EnrichmentRepository,
    I: ArtistIdentityProvider,
    P: ArtistImageProvider,
    L: LyricsProvider,
    S: ArtistImageStore,
    C: Clock,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        auth: A,
        repo: R,
        identity: I,
        images: P,
        lyrics: L,
        store: S,
        clock: C,
        settings: MetadataSettings,
    ) -> Self {
        Self {
            auth,
            repo,
            identity,
            images,
            lyrics,
            store,
            clock,
            settings,
        }
    }

    /// Enrich everything in `scope`.
    ///
    /// Only two things fail this command: a caller who is not authenticated,
    /// and enrichment not being available. Everything after that is recorded
    /// rather than raised — a service being down, rate-limiting, or simply
    /// having nothing is ordinary, and a run that aborted on the first
    /// unreachable host would never get through a library (design section 5).
    pub async fn enrich(
        &self,
        scope: EnrichmentScope,
        token: &str,
    ) -> Result<EnrichmentReport, DomainError> {
        self.auth.authenticate(token).await?;

        // Refused before anything is read, and refused with the reason. A
        // caller told only "no" cannot tell the owner whether to switch the
        // feature on or to fill in a contact.
        if let Some(reason) = self.settings.unavailable_reason() {
            return Err(match reason {
                MetadataUnavailable::Disabled => DomainError::InvalidState,
                MetadataUnavailable::ContactMissing => DomainError::InvalidInput(
                    "metadata.contact must be set before enrichment can run".to_string(),
                ),
            });
        }

        let candidates = self.repo.candidates(&scope).await?;
        let mut report = EnrichmentReport::default();

        // Artists already handled in *this* run. A library is mostly a small
        // number of artists across many tracks, so without this an album of
        // twelve tracks would ask MusicBrainz twelve times for one artist —
        // twelve seconds at the rate limit, for one answer.
        let mut seen_artists: HashMap<String, ()> = HashMap::new();

        for candidate in candidates {
            self.enrich_artist_image(&candidate, &mut seen_artists, &mut report)
                .await?;
            self.enrich_lyrics(&candidate, &scope, &mut report).await?;
        }

        Ok(report)
    }

    /// Look up one candidate's artist image, unless it is already settled.
    async fn enrich_artist_image(
        &self,
        candidate: &EnrichmentCandidate,
        seen: &mut HashMap<String, ()>,
        report: &mut EnrichmentReport,
    ) -> Result<(), DomainError> {
        let Some(artist) = candidate.image_artist() else {
            // No artist tag at all. Searching on nothing would return
            // whatever is most popular, so this is skipped rather than asked.
            report.skip();
            return Ok(());
        };
        let artist = artist.to_string();

        if seen.contains_key(&artist) {
            return Ok(());
        }

        if let Some(stored) = self.repo.artist_image(&artist).await? {
            if stored.outcome.is_settled() {
                seen.insert(artist, ());
                report.skip();
                return Ok(());
            }
        }

        let outcome = self.fetch_artist_image(&artist).await;
        self.repo.put_artist_image(outcome.clone()).await?;
        seen.insert(artist, ());
        report.record(outcome.outcome);

        Ok(())
    }

    /// The image row for `artist`, whatever the lookup concluded.
    ///
    /// Returns a row rather than a `Result` of one: every path here — found,
    /// nothing, below threshold, service down — is a conclusion worth
    /// storing, and the only way to be asked again is to store `Failed`.
    async fn fetch_artist_image(&self, artist: &str) -> ArtistImage {
        let now = self.clock.now();
        let row = |outcome, mbid, source_url, image_path| ArtistImage {
            artist_name: artist.to_string(),
            mbid,
            source_url,
            image_path,
            outcome,
            fetched_at: now,
        };

        let matched = match self.identity.find_artist(artist).await {
            Ok(Some(matched)) => matched,
            Ok(None) => return row(EnrichmentOutcome::NotFound, None, None, None),
            Err(_) => return row(EnrichmentOutcome::Failed, None, None, None),
        };

        // The threshold, and the reason this is not "take the first result".
        // MusicBrainz answers a misspelled name with a low-scoring hit rather
        // than an empty list, and a confidently wrong face on an artist page
        // is worse than a blank one. The rejected match's id is still stored,
        // so a wrong-looking library can be explained.
        if matched.score < MIN_ARTIST_SCORE {
            return row(EnrichmentOutcome::Rejected, Some(matched.mbid), None, None);
        }

        let asset = match self.images.image_for(&matched.mbid).await {
            Ok(Some(asset)) => asset,
            Ok(None) => return row(EnrichmentOutcome::NotFound, Some(matched.mbid), None, None),
            Err(_) => return row(EnrichmentOutcome::Failed, Some(matched.mbid), None, None),
        };

        match self.store.store(&matched.mbid, &asset).await {
            Ok(path) => row(
                EnrichmentOutcome::Found,
                Some(matched.mbid),
                Some(asset.source_url),
                Some(path),
            ),
            // The bytes arrived but could not be written. `Failed`, so a later
            // run tries again — a full disk is temporary, and recording this
            // as `NotFound` would permanently deny an artist a photo that
            // exists.
            Err(_) => row(EnrichmentOutcome::Failed, Some(matched.mbid), None, None),
        }
    }

    /// Look up one candidate's lyrics, unless they are already settled.
    async fn enrich_lyrics(
        &self,
        candidate: &EnrichmentCandidate,
        scope: &EnrichmentScope,
        report: &mut EnrichmentReport,
    ) -> Result<(), DomainError> {
        if !candidate.lyrics_searchable() {
            report.skip();
            return Ok(());
        }

        // `Pending` has already excluded settled rows in SQL; the explicit
        // scopes deliberately have not, because naming one track or one
        // artist is the caller asking for it to be done again.
        if matches!(scope, EnrichmentScope::Pending) {
            if let Some(stored) = self.repo.lyrics(candidate.file_uuid).await? {
                if stored.outcome.is_settled() {
                    report.skip();
                    return Ok(());
                }
            }
        }

        let row = self.fetch_lyrics(candidate).await;
        let outcome = row.outcome;

        match self.repo.put_lyrics(row).await {
            Ok(()) => report.record(outcome),
            // The file went away between being listed as a candidate and the
            // write — an owner purging something while a run over their whole
            // library is in flight, which is a long window. There is nothing
            // to record it against and nothing wrong with the run, so it is
            // skipped rather than raised: aborting thousands of remaining
            // tracks because one of them was deleted is exactly the failure
            // design section 5 rules out.
            Err(DomainError::NotFound) => report.skip(),
            Err(other) => return Err(other),
        }

        Ok(())
    }

    async fn fetch_lyrics(&self, candidate: &EnrichmentCandidate) -> TrackLyrics {
        let now = self.clock.now();
        let row = |outcome, plain, synced, source| TrackLyrics {
            file_uuid: candidate.file_uuid,
            mbid: None,
            plain,
            synced,
            source,
            outcome,
            fetched_at: now,
        };

        let query = LyricsQuery {
            title: candidate.title.clone().unwrap_or_default(),
            artist: candidate
                .artist
                .clone()
                .or_else(|| candidate.album_artist.clone())
                .unwrap_or_default(),
            album: candidate.album.clone(),
            duration_seconds: candidate.duration_seconds,
        };

        match self.lyrics.lyrics_for(&query).await {
            Ok(Some(found)) => row(
                EnrichmentOutcome::Found,
                found.plain,
                found.synced,
                Some(found.source),
            ),
            Ok(None) => row(EnrichmentOutcome::NotFound, None, None, None),
            Err(_) => row(EnrichmentOutcome::Failed, None, None, None),
        }
    }
}
