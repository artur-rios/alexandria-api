use chrono::{DateTime, Utc};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use uuid::Uuid;

use crate::catalog::model::FileType;
use crate::errors::DomainError;
use crate::plays::model::{AlbumPlays, ArtistPlays, GenrePlays, MusicStats, PlayEvent, TrackPlays};

/// The credit a track is ranked under: its album artist where it has one,
/// its artist otherwise, and NULL when it carries neither.
///
/// Written once as a constant because the artist ranking groups by it, the
/// album ranking checks whether an album's tracks agree on it, and the two
/// must be the same expression — an album credited one way and an artist
/// credited another would be two answers to the same question on one
/// screen. Blank tags are folded into NULL here (`NULLIF(TRIM(...), '')`)
/// so a file tagged with an empty string ranks as untagged rather than as
/// an artist whose name is nothing.
const CREDIT: &str = "COALESCE(NULLIF(TRIM(a.album_artist), ''), NULLIF(TRIM(a.artist), ''))";

/// Play history repository port. The handlers depend on this trait so
/// their decision logic (auth, validation, the clock) is unit-tested
/// against an in-memory fake with no database (Testing Specification
/// §6.2).
#[allow(async_fn_in_trait)]
pub trait PlayRepository: Send + Sync {
    /// Record that the track identified by `file_uuid` was played at
    /// `played_at`.
    ///
    /// `NotFound` when the uuid does not resolve to a file; `InvalidInput`
    /// when it resolves to something that is not audio — the statistics
    /// are of music, and a video's viewing is the watchlists' business
    /// (UC-21), with its own progress model.
    ///
    /// Deliberately not idempotent and deliberately unconstrained: playing
    /// the same track twice is two plays, which is the entire point of
    /// counting them.
    async fn record(
        &self,
        file_uuid: Uuid,
        played_at: DateTime<Utc>,
    ) -> Result<PlayEvent, DomainError>;

    /// What was played most, each ranking cut to `limit` rows.
    ///
    /// Five queries — a summary and one per ranking — regardless of how
    /// much has been played, the same constant-query-count property
    /// `browse_batching.rs` pins for the listing. Never one query per
    /// track, artist, or album.
    ///
    /// The tags are read live, joined at ranking time rather than
    /// snapshotted onto the play rows; see `TrackPlays` for why that
    /// direction was chosen.
    async fn stats(&self, limit: i64) -> Result<MusicStats, DomainError>;
}

#[derive(Clone)]
pub struct SqlitePlayRepository {
    pool: SqlitePool,
}

impl SqlitePlayRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

impl PlayRepository for SqlitePlayRepository {
    async fn record(
        &self,
        file_uuid: Uuid,
        played_at: DateTime<Utc>,
    ) -> Result<PlayEvent, DomainError> {
        let row = sqlx::query("SELECT id, type FROM files WHERE uuid = ?")
            .bind(file_uuid.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(DomainError::NotFound)?;

        let file_id: i64 = row.try_get("id")?;
        let file_type: String = row.try_get("type")?;
        if file_type != FileType::Audio.as_str() {
            return Err(DomainError::InvalidInput(format!(
                "file {file_uuid} is not audio"
            )));
        }

        sqlx::query("INSERT INTO play_events (file_id, played_at) VALUES (?, ?)")
            .bind(file_id)
            .bind(played_at.to_rfc3339())
            .execute(&self.pool)
            .await?;

        Ok(PlayEvent {
            file_uuid,
            played_at,
        })
    }

    async fn stats(&self, limit: i64) -> Result<MusicStats, DomainError> {
        // Query 1: the summary. `MIN`/`MAX` over `played_at` rather than an
        // ordered read of the events: the column is RFC 3339 in UTC for
        // every row this code writes, so lexical order is chronological
        // order, and the index on it makes both ends a lookup.
        let summary = sqlx::query(
            "SELECT COUNT(*) AS total_plays, COUNT(DISTINCT file_id) AS distinct_tracks, \
             MIN(played_at) AS first_played_at, MAX(played_at) AS last_played_at \
             FROM play_events",
        )
        .fetch_one(&self.pool)
        .await?;

        let total_plays: i64 = summary.try_get("total_plays")?;
        let distinct_tracks: i64 = summary.try_get("distinct_tracks")?;
        let first_played_at: Option<String> = summary.try_get("first_played_at")?;
        let last_played_at: Option<String> = summary.try_get("last_played_at")?;

        // Query 2: the tracks. A LEFT JOIN, unlike the three rankings
        // below: an untagged track has no `audio_files` row to speak of and
        // still belongs here under its filename, where it belongs in none
        // of the others (`MusicStats`). The INNER JOIN against `files` is
        // what makes a purged track's plays unreachable — and the
        // migration's cascade is what stops them existing at all.
        let track_rows = sqlx::query(
            "SELECT f.uuid AS file_uuid, \
             COALESCE(NULLIF(TRIM(a.title), ''), f.name) AS title, \
             NULLIF(TRIM(a.artist), '') AS artist, \
             NULLIF(TRIM(a.album), '') AS album, \
             COUNT(*) AS plays, MAX(pe.played_at) AS last_played_at \
             FROM play_events pe \
             JOIN files f ON f.id = pe.file_id \
             LEFT JOIN audio_files a ON a.file_id = pe.file_id \
             GROUP BY pe.file_id \
             ORDER BY plays DESC, title ASC, file_uuid ASC \
             LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        let mut top_tracks = Vec::with_capacity(track_rows.len());
        for row in track_rows {
            let file_uuid: String = row.try_get("file_uuid")?;
            let title: String = row.try_get("title")?;
            let artist: Option<String> = row.try_get("artist")?;
            let album: Option<String> = row.try_get("album")?;
            let plays: i64 = row.try_get("plays")?;
            let last: String = row.try_get("last_played_at")?;
            top_tracks.push(TrackPlays {
                file_uuid: Uuid::parse_str(&file_uuid).map_err(|err| {
                    DomainError::internal(format!("corrupt played file uuid: {err}"))
                })?,
                title,
                artist,
                album,
                plays,
                last_played_at: parse_played_at(&last)?,
            });
        }

        // Query 3: the artists. `HAVING credit IS NOT NULL` rather than a
        // `WHERE` on the same expression -- the alias is what the grouping
        // is by, and repeating the expression in two clauses is how the
        // two would eventually stop matching.
        let artist_sql = format!(
            "SELECT {CREDIT} AS credit, COUNT(*) AS plays, \
             COUNT(DISTINCT pe.file_id) AS tracks \
             FROM play_events pe \
             JOIN audio_files a ON a.file_id = pe.file_id \
             GROUP BY credit \
             HAVING credit IS NOT NULL \
             ORDER BY plays DESC, credit ASC \
             LIMIT ?"
        );
        let artist_rows = sqlx::query(sqlx::AssertSqlSafe(artist_sql))
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;

        let mut top_artists = Vec::with_capacity(artist_rows.len());
        for row in artist_rows {
            top_artists.push(ArtistPlays {
                artist: row.try_get("credit")?,
                plays: row.try_get("plays")?,
                tracks: row.try_get("tracks")?,
            });
        }

        // Query 4: the albums. `COUNT(DISTINCT credit) = 1` is the "every
        // played track that names an artist agrees" test -- `COUNT
        // DISTINCT` skips NULLs, so an album where half the tracks are
        // uncredited and the rest all say the same name still answers with
        // that name, and only a genuine disagreement answers with none.
        let album_sql = format!(
            "SELECT NULLIF(TRIM(a.album), '') AS album, COUNT(*) AS plays, \
             CASE WHEN COUNT(DISTINCT {CREDIT}) = 1 THEN MIN({CREDIT}) END AS credit \
             FROM play_events pe \
             JOIN audio_files a ON a.file_id = pe.file_id \
             GROUP BY album \
             HAVING album IS NOT NULL \
             ORDER BY plays DESC, album ASC \
             LIMIT ?"
        );
        let album_rows = sqlx::query(sqlx::AssertSqlSafe(album_sql))
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;

        let mut top_albums = Vec::with_capacity(album_rows.len());
        for row in album_rows {
            top_albums.push(AlbumPlays {
                album: row.try_get("album")?,
                artist: row.try_get("credit")?,
                plays: row.try_get("plays")?,
            });
        }

        // Query 5: the genres.
        let genre_rows = sqlx::query(
            "SELECT NULLIF(TRIM(a.genre), '') AS genre, COUNT(*) AS plays \
             FROM play_events pe \
             JOIN audio_files a ON a.file_id = pe.file_id \
             GROUP BY genre \
             HAVING genre IS NOT NULL \
             ORDER BY plays DESC, genre ASC \
             LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        let mut top_genres = Vec::with_capacity(genre_rows.len());
        for row in genre_rows {
            top_genres.push(GenrePlays {
                genre: row.try_get("genre")?,
                plays: row.try_get("plays")?,
            });
        }

        Ok(MusicStats {
            total_plays,
            distinct_tracks,
            first_played_at: first_played_at
                .map(|value| parse_played_at(&value))
                .transpose()?,
            last_played_at: last_played_at
                .map(|value| parse_played_at(&value))
                .transpose()?,
            top_tracks,
            top_artists,
            top_albums,
            top_genres,
        })
    }
}

fn parse_played_at(value: &str) -> Result<DateTime<Utc>, DomainError> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|_| {
            DomainError::internal(format!(
                "corrupt play event: unparseable played_at {value:?}"
            ))
        })
}
