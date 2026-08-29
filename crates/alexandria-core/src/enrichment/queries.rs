//! Reading enrichment back.

use std::path::PathBuf;

use uuid::Uuid;

use crate::auth::AuthService;
use crate::enrichment::model::{ArtistImage, TrackLyrics};
use crate::enrichment::repos::EnrichmentRepository;
use crate::errors::DomainError;

/// What a client shows beside a playing track.
///
/// One call answering both halves, because a player showing a track needs
/// both at the same moment and two round trips would be two chances for one
/// of them to arrive late. Either half may be absent — most of a real
/// library will have one, some, or neither.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackEnrichmentView {
    pub artist_image: Option<ArtistImage>,
    pub lyrics: Option<TrackLyrics>,
}

/// Read what enrichment has stored for one file.
pub struct ReadEnrichmentHandler<A, R> {
    auth: A,
    repo: R,
    /// Where artist images live, so `image_path` can be answered as
    /// something the caller can actually open.
    ///
    /// Stored relative and answered absolute, deliberately. Relative is what
    /// survives the cache directory being moved or the catalog being opened
    /// on another machine; absolute is what a client can hand to an image
    /// widget. A client has no way to learn this directory otherwise — it is
    /// the core's configuration, not theirs — so resolving it here is the
    /// difference between a usable answer and a string they must guess at.
    image_root: PathBuf,
}

impl<A, R> ReadEnrichmentHandler<A, R>
where
    A: AuthService,
    R: EnrichmentRepository,
{
    pub fn new(auth: A, repo: R, image_root: impl Into<PathBuf>) -> Self {
        Self {
            auth,
            repo,
            image_root: image_root.into(),
        }
    }

    /// The stored image and lyrics for `file_uuid`.
    ///
    /// `artist_name` is passed in rather than resolved here because the
    /// caller — a player that already has the track on screen — has already
    /// read the file's tags, and re-reading them would be a second query for
    /// a fact it is holding.
    ///
    /// A row whose outcome is not `Found` reads as absent. The distinction
    /// between "looked up and found nothing" and "never looked up" is real
    /// and is what makes the run resumable, but it is bookkeeping for the
    /// command, not something a player has any use for — it has nothing
    /// different to draw for the two.
    pub async fn read(
        &self,
        file_uuid: Uuid,
        artist_name: Option<&str>,
        token: &str,
    ) -> Result<TrackEnrichmentView, DomainError> {
        self.auth.authenticate(token).await?;

        let artist_image = match artist_name {
            Some(name) if !name.trim().is_empty() => self
                .repo
                .artist_image(name.trim())
                .await?
                .filter(|image| image.image_path.is_some())
                .map(|mut image| {
                    image.image_path = image.image_path.map(|relative| {
                        self.image_root
                            .join(relative)
                            .to_string_lossy()
                            .into_owned()
                    });
                    image
                }),
            _ => None,
        };

        let lyrics = self
            .repo
            .lyrics(file_uuid)
            .await?
            .filter(|found| found.plain.is_some() || found.synced.is_some());

        Ok(TrackEnrichmentView {
            artist_image,
            lyrics,
        })
    }
}
