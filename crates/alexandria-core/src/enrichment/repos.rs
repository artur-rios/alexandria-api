use chrono::Utc;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use uuid::Uuid;

use crate::enrichment::model::{ArtistImage, EnrichmentOutcome, TrackLyrics};
use crate::errors::{DomainError, WRITE_TX};

/// One audio file enrichment has something to ask about.
///
/// Everything a lookup needs and nothing else — deliberately not `FileView`,
/// which carries the whole catalog record. This is what the enrichment run
/// iterates, and a run over a whole library holds thousands of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrichmentCandidate {
    pub file_uuid: Uuid,
    pub title: Option<String>,
    pub artist: Option<String>,
    /// The album artist, which is who the *record* is by and therefore whose
    /// photograph belongs against it — not `artist`, which names whoever
    /// performed this track and would give a compilation twelve artist
    /// images instead of one.
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub duration_seconds: Option<u32>,
}

impl EnrichmentCandidate {
    /// The artist whose image this track should carry, or `None` when the
    /// tags name neither.
    ///
    /// Album artist first, performer as the fallback — the same precedence
    /// the music browsing area applies, so a library browses and enriches
    /// under one name rather than two.
    pub fn image_artist(&self) -> Option<&str> {
        self.album_artist
            .as_deref()
            .or(self.artist.as_deref())
            .map(str::trim)
            .filter(|name| !name.is_empty())
    }

    /// Whether a lyrics lookup can even be attempted.
    ///
    /// Both a title and an artist are required. Searching on one of them
    /// alone returns whatever is most popular under that word, which is how
    /// a library ends up with confidently wrong lyrics against a track — and
    /// a wrong answer here is worse than none, because nothing on screen
    /// says it might be wrong.
    pub fn lyrics_searchable(&self) -> bool {
        let filled = |value: &Option<String>| {
            value
                .as_deref()
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
        };
        filled(&self.title) && (filled(&self.artist) || filled(&self.album_artist))
    }
}

/// Enrichment repository port.
///
/// The command depends on this rather than on `SqlitePool` so its decisions —
/// what is skipped, what is recorded, what a settled outcome means — are unit
/// tested against an in-memory fake, with no database and no network
/// (Testing Specification §6.2).
#[allow(async_fn_in_trait)]
pub trait EnrichmentRepository: Send + Sync {
    /// The audio files in scope for a run.
    ///
    /// `Pending` scope excludes files whose lyrics outcome is already
    /// settled; the other two scopes return their files whatever the stored
    /// outcome, because asking for one track or one artist explicitly is the
    /// caller saying "do it again".
    async fn candidates(
        &self,
        scope: &crate::enrichment::model::EnrichmentScope,
    ) -> Result<Vec<EnrichmentCandidate>, DomainError>;

    /// The stored artist image row, whatever it concluded.
    async fn artist_image(&self, artist_name: &str) -> Result<Option<ArtistImage>, DomainError>;

    /// Write (or replace) an artist image row.
    async fn put_artist_image(&self, image: ArtistImage) -> Result<(), DomainError>;

    /// The stored lyrics row for a file, whatever it concluded.
    async fn lyrics(&self, file_uuid: Uuid) -> Result<Option<TrackLyrics>, DomainError>;

    /// Write (or replace) a lyrics row.
    async fn put_lyrics(&self, lyrics: TrackLyrics) -> Result<(), DomainError>;
}

/// The Sqlite implementation.
pub struct SqliteEnrichmentRepository {
    pool: SqlitePool,
}

impl SqliteEnrichmentRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

impl EnrichmentRepository for SqliteEnrichmentRepository {
    async fn candidates(
        &self,
        scope: &crate::enrichment::model::EnrichmentScope,
    ) -> Result<Vec<EnrichmentCandidate>, DomainError> {
        use crate::enrichment::model::EnrichmentScope;

        // Written out in full, three times, rather than composed from a
        // shared prefix: sqlx 0.9 accepts only `&'static str` here, and
        // building the statement at runtime to dodge that would be defeating
        // the check that stops SQL injection rather than satisfying it. The
        // repetition is the cost of the guarantee.
        let rows = match scope {
            EnrichmentScope::File(uuid) => {
                sqlx::query(
                    "SELECT f.uuid AS uuid, a.title, a.artist, a.album_artist, a.album,
                            a.duration_seconds
                     FROM files f
                     JOIN audio_files a ON a.file_id = f.id
                     WHERE f.state = 'active' AND f.uuid = ?",
                )
                .bind(uuid.to_string())
                .fetch_all(&self.pool)
                .await
            }
            EnrichmentScope::Artist(name) => {
                sqlx::query(
                    "SELECT f.uuid AS uuid, a.title, a.artist, a.album_artist, a.album,
                            a.duration_seconds
                     FROM files f
                     JOIN audio_files a ON a.file_id = f.id
                     WHERE f.state = 'active'
                       AND (a.album_artist = ? OR (a.album_artist IS NULL AND a.artist = ?))",
                )
                .bind(name)
                .bind(name)
                .fetch_all(&self.pool)
                .await
            }
            // A file is pending while EITHER of its two facts is unsettled.
            //
            // Filtering on the lyrics outcome alone looked right and was not:
            // one run where MusicBrainz is down and LRCLIB is up settles
            // every file's lyrics and fails every artist image, and those
            // images are then unreachable by any number of later runs — the
            // exact resumability the `outcome` column exists to provide,
            // defeated. The image half is keyed by artist, so it is checked
            // through the same album-artist-then-performer fallback the
            // handler applies.
            //
            // Settled is named positively (`IN (…)`) rather than as `<>
            // 'failed'`, so an unrecognized value is retried here exactly as
            // `EnrichmentOutcome::from_stored` retries it — see
            // `SETTLED_OUTCOMES_SQL`.
            EnrichmentScope::Pending => {
                sqlx::query(
                    "SELECT f.uuid AS uuid, a.title, a.artist, a.album_artist, a.album,
                            a.duration_seconds
                     FROM files f
                     JOIN audio_files a ON a.file_id = f.id
                     WHERE f.state = 'active'
                       AND (
                           NOT EXISTS (
                               SELECT 1 FROM track_lyrics l
                               WHERE l.file_id = f.id
                                 AND l.outcome IN ('found', 'notFound', 'rejected')
                           )
                           OR NOT EXISTS (
                               SELECT 1 FROM artist_images i
                               WHERE i.artist_name = COALESCE(
                                         NULLIF(TRIM(a.album_artist), ''),
                                         TRIM(a.artist)
                                     )
                                 AND i.outcome IN ('found', 'notFound', 'rejected')
                           )
                       )",
                )
                .fetch_all(&self.pool)
                .await
            }
        }
        .map_err(|e| DomainError::Disk(e.to_string()))?;

        rows.into_iter()
            .map(|row| {
                let uuid: String = row
                    .try_get("uuid")
                    .map_err(|e| DomainError::Disk(e.to_string()))?;
                Ok(EnrichmentCandidate {
                    file_uuid: Uuid::parse_str(&uuid)
                        .map_err(|e| DomainError::Disk(e.to_string()))?,
                    title: row.try_get("title").ok(),
                    artist: row.try_get("artist").ok(),
                    album_artist: row.try_get("album_artist").ok(),
                    album: row.try_get("album").ok(),
                    // Stored as REAL and sent to LRCLIB as whole seconds,
                    // which is the unit their API takes. Rounded rather than
                    // truncated: a 245.7-second track is 246 seconds to
                    // anyone who timed it, and truncating would put every
                    // track systematically one second short of the value the
                    // provider holds.
                    duration_seconds: row
                        .try_get::<Option<f64>, _>("duration_seconds")
                        .ok()
                        .flatten()
                        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
                        .map(|seconds| seconds.round() as u32),
                })
            })
            .collect()
    }

    async fn artist_image(&self, artist_name: &str) -> Result<Option<ArtistImage>, DomainError> {
        let row = sqlx::query(
            "SELECT artist_name, mbid, source_url, image_path, outcome, fetched_at
             FROM artist_images WHERE artist_name = ?",
        )
        .bind(artist_name)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DomainError::Disk(e.to_string()))?;

        let Some(row) = row else { return Ok(None) };
        let outcome: String = row
            .try_get("outcome")
            .map_err(|e| DomainError::Disk(e.to_string()))?;
        let fetched_at: chrono::DateTime<Utc> = row
            .try_get("fetched_at")
            .map_err(|e| DomainError::Disk(e.to_string()))?;

        Ok(Some(ArtistImage {
            artist_name: row
                .try_get("artist_name")
                .map_err(|e| DomainError::Disk(e.to_string()))?,
            mbid: row.try_get("mbid").ok(),
            source_url: row.try_get("source_url").ok(),
            image_path: row.try_get("image_path").ok(),
            outcome: EnrichmentOutcome::from_stored(&outcome),
            fetched_at,
        }))
    }

    async fn put_artist_image(&self, image: ArtistImage) -> Result<(), DomainError> {
        let mut tx = self
            .pool
            .begin_with(WRITE_TX)
            .await
            .map_err(|e| DomainError::Disk(e.to_string()))?;

        // Upsert on the unique artist name: a re-run replaces the previous
        // conclusion rather than accumulating one row per attempt.
        sqlx::query(
            "INSERT INTO artist_images (artist_name, mbid, source_url, image_path, outcome, fetched_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(artist_name) DO UPDATE SET
                 mbid = excluded.mbid,
                 source_url = excluded.source_url,
                 image_path = excluded.image_path,
                 outcome = excluded.outcome,
                 fetched_at = excluded.fetched_at",
        )
        .bind(&image.artist_name)
        .bind(&image.mbid)
        .bind(&image.source_url)
        .bind(&image.image_path)
        .bind(image.outcome.as_str())
        .bind(image.fetched_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| DomainError::Disk(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| DomainError::Disk(e.to_string()))
    }

    async fn lyrics(&self, file_uuid: Uuid) -> Result<Option<TrackLyrics>, DomainError> {
        let row = sqlx::query(
            "SELECT f.uuid AS file_uuid, l.mbid, l.plain, l.synced, l.source, l.outcome, l.fetched_at
             FROM track_lyrics l
             JOIN files f ON f.id = l.file_id
             WHERE f.uuid = ?",
        )
        .bind(file_uuid.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DomainError::Disk(e.to_string()))?;

        let Some(row) = row else { return Ok(None) };
        let outcome: String = row
            .try_get("outcome")
            .map_err(|e| DomainError::Disk(e.to_string()))?;
        let fetched_at: chrono::DateTime<Utc> = row
            .try_get("fetched_at")
            .map_err(|e| DomainError::Disk(e.to_string()))?;

        Ok(Some(TrackLyrics {
            file_uuid,
            mbid: row.try_get("mbid").ok(),
            plain: row.try_get("plain").ok(),
            synced: row.try_get("synced").ok(),
            source: row.try_get("source").ok(),
            outcome: EnrichmentOutcome::from_stored(&outcome),
            fetched_at,
        }))
    }

    async fn put_lyrics(&self, lyrics: TrackLyrics) -> Result<(), DomainError> {
        let mut tx = self
            .pool
            .begin_with(WRITE_TX)
            .await
            .map_err(|e| DomainError::Disk(e.to_string()))?;

        // The file id is resolved inside the same transaction as the write,
        // so a file purged between the two cannot leave a row pointing at
        // nothing. `track_lyrics` has no foreign key (SQLite cannot add one
        // through ALTER TABLE), so nothing else would catch that.
        let file_id: Option<i64> = sqlx::query_scalar("SELECT id FROM files WHERE uuid = ?")
            .bind(lyrics.file_uuid.to_string())
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| DomainError::Disk(e.to_string()))?;
        let Some(file_id) = file_id else {
            return Err(DomainError::NotFound);
        };

        sqlx::query(
            "INSERT INTO track_lyrics (file_id, mbid, plain, synced, source, outcome, fetched_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(file_id) DO UPDATE SET
                 mbid = excluded.mbid,
                 plain = excluded.plain,
                 synced = excluded.synced,
                 source = excluded.source,
                 outcome = excluded.outcome,
                 fetched_at = excluded.fetched_at",
        )
        .bind(file_id)
        .bind(&lyrics.mbid)
        .bind(&lyrics.plain)
        .bind(&lyrics.synced)
        .bind(&lyrics.source)
        .bind(lyrics.outcome.as_str())
        .bind(lyrics.fetched_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| DomainError::Disk(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| DomainError::Disk(e.to_string()))
    }
}
