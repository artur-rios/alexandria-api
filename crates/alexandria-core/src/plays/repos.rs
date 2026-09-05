use chrono::{DateTime, Utc};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use uuid::Uuid;

use crate::catalog::model::FileType;
use crate::errors::DomainError;
use crate::plays::model::{AlbumPlays, ArtistPlays, GenrePlays, MusicStats, PlayEvent, TrackPlays};

/// The credit every ranking is built on, as a set of common table
/// expressions the artist and album queries both select from.
///
/// Written once because the artist ranking groups by the credit and the
/// album ranking groups by the album *and* the credit, and an album credited
/// one way while its artist is credited another would be two answers to the
/// same question on one screen. Blank tags are folded into NULL
/// (`NULLIF(TRIM(...), '')`) so a file tagged with an empty string ranks as
/// untagged rather than as an artist whose name is nothing.
///
/// **Four answers, in order of how much they know**, which is the same
/// precedence a client's own album browsing applies and is why this is no
/// longer a one-line `COALESCE`:
///
/// 1. the track's own `album_artist`, which is the record answering for
///    itself;
/// 2. the commonest `album_artist` on the *rest of that album*, for a track
///    that carries none — files are half-tagged all the time, and one track
///    naming the record settles it for the ones that say nothing;
/// 3. the commonest performer on the album, with no `album_artist` anywhere:
///    whoever most of the record is by, so a guest on one track does not
///    become an artist in their own right;
/// 4. the track's own performer, for a track belonging to no album.
///
/// Rules 2 and 3 are why the album ranking can group by the pair at all. The
/// pair is what an album *is* — `Greatest Hits` names a hundred records, and
/// grouping by title alone summed two of them into one row whose plays
/// belonged to neither — but the naive pair, `album_artist` or performer per
/// track, splits an ordinary record across its guests and splits a
/// half-tagged one down the middle. Deriving the record's own credit first
/// is what makes the pair right rather than merely different.
///
/// Ties break alphabetically, matching that client exactly: a ranking that
/// reordered itself depending on which row the planner reached first would
/// be a ranking the owner cannot rely on.
///
/// The commonest is counted in *tracks*, not in plays: the question is which
/// artist most of the record is by, and a record where one track was played
/// forty times is not a record by that track's guest.
///
/// The judgement in rule 3 is worth naming as one, and it is the client's
/// too: a genuine various-artists compilation carrying no `album_artist`
/// anywhere lands under whichever performer has the most tracks on it. That
/// is a worse answer for that one record than naming none — and a far better
/// one for every ordinary album with a guest on it, which is what most
/// libraries are made of. An owner who disagrees has the tag, and rule 1
/// gives it precedence over everything here.
const CREDITED: &str = "\
    WITH played AS ( \
        SELECT pe.file_id AS file_id, \
               NULLIF(TRIM(a.album), '') AS album, \
               NULLIF(TRIM(a.album_artist), '') AS tagged, \
               NULLIF(TRIM(a.artist), '') AS performer \
        FROM play_events pe \
        JOIN audio_files a ON a.file_id = pe.file_id \
    ), \
    tagged_seat AS ( \
        SELECT album, tagged AS name, \
               ROW_NUMBER() OVER (PARTITION BY album \
                                  ORDER BY COUNT(DISTINCT file_id) DESC, tagged ASC) AS seat \
        FROM played \
        WHERE album IS NOT NULL AND tagged IS NOT NULL \
        GROUP BY album, tagged \
    ), \
    performer_seat AS ( \
        SELECT album, performer AS name, \
               ROW_NUMBER() OVER (PARTITION BY album \
                                  ORDER BY COUNT(DISTINCT file_id) DESC, performer ASC) AS seat \
        FROM played \
        WHERE album IS NOT NULL AND performer IS NOT NULL \
        GROUP BY album, performer \
    ), \
    credited AS ( \
        SELECT p.file_id AS file_id, \
               p.album AS album, \
               COALESCE(p.tagged, t.name, r.name, p.performer) AS credit \
        FROM played p \
        LEFT JOIN tagged_seat t ON t.album = p.album AND t.seat = 1 \
        LEFT JOIN performer_seat r ON r.album = p.album AND r.seat = 1 \
    ) ";

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

        // Query 3: the artists, over the shared `credited` expression --
        // see `CREDITED` for what a credit is and why it is derived per
        // album rather than read straight off each track. `HAVING credit IS
        // NOT NULL` rather than a `WHERE`: the alias is what the grouping is
        // by, and naming the same thing in two clauses is how the two would
        // eventually stop matching.
        let artist_sql = format!(
            "{CREDITED} \
             SELECT credit, COUNT(*) AS plays, COUNT(DISTINCT file_id) AS tracks \
             FROM credited \
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

        // Query 4: the albums, grouped by the *pair* -- title and credit --
        // over the same `credited` expression the artists rank over, so an
        // album and its artist are credited alike.
        //
        // The pair rather than the title alone, which is what this was.
        // `Greatest Hits` names a hundred different records, and grouping by
        // title summed two of them into one row with their plays added
        // together and an artist of NULL, because the two records disagreed
        // about the credit. A number that is the sum of two unrelated records
        // is wrong rather than merely coarse, and it was drawn as confidently
        // as a right one. A client's own album browsing keys by the pair for
        // exactly this reason, and one product must not answer "which album
        // is this" two ways on two screens.
        //
        // The pair works here only because `CREDITED` derives the record's
        // own credit first. Grouped by the credit read off each track, an
        // ordinary album with a guest on it would split across its guests and
        // a half-tagged one would split down the middle -- which would be a
        // different wrong answer, not a fix.
        //
        // `credit` can still be NULL: an album none of whose played tracks
        // names anyone. NULL is its own group in SQLite's `GROUP BY`, so
        // those rank together under the title, which is as much as the
        // catalog can say about them.
        let album_sql = format!(
            "{CREDITED} \
             SELECT album, credit, COUNT(*) AS plays \
             FROM credited \
             GROUP BY album, credit \
             HAVING album IS NOT NULL \
             ORDER BY plays DESC, album ASC, credit ASC \
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
