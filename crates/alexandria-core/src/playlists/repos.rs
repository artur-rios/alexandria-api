use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use uuid::Uuid;

use crate::catalog::model::{File, FileState, FileType, FileView, SubtypeMetadata};
use crate::errors::{DomainError, WRITE_TX};
use crate::playlists::model::{NewPlaylist, Playlist, PlaylistEntry, PlaylistTrack};

/// The largest number of `?` placeholders one batched `WHERE file_id IN
/// (…)` query binds, mirroring `catalog::repos::MAX_SQLITE_PARAMS` (see
/// that constant's doc comment for why 900, not SQLite's actual compiled-in
/// ceiling). Duplicated here rather than imported because the catalog's
/// constant is private to its own module — playlists needs the same number,
/// not a dependency on the catalog repository's internals.
const MAX_SQLITE_PARAMS: usize = 900;

/// Playlists repository port. The create handler depends on this trait so
/// its decision logic (validation, uuid minting) is unit-tested against an
/// in-memory fake with no database (Testing Specification §6.2). The
/// Sqlite implementation persists `playlists` rows; `playlist_entries`
/// methods arrive with the use cases that need them.
#[allow(async_fn_in_trait)]
pub trait PlaylistRepository: Send + Sync {
    /// Persist a new playlist and return the stored record. The caller has
    /// already validated the name.
    async fn insert_playlist(&self, new_playlist: NewPlaylist) -> Result<Playlist, DomainError>;

    /// Look a playlist up by its public uuid. `None` when no such playlist
    /// exists.
    async fn find_by_uuid(&self, uuid: Uuid) -> Result<Option<Playlist>, DomainError>;

    /// Every persisted playlist, ordered by name.
    async fn list_all(&self) -> Result<Vec<Playlist>, DomainError>;

    /// Rename the playlist identified by `uuid` to `name` and return the
    /// updated record. The caller has already validated the name and
    /// confirmed the playlist exists.
    async fn rename_playlist(&self, uuid: Uuid, name: String) -> Result<Playlist, DomainError>;

    /// Delete the playlist identified by `uuid`, along with every
    /// `playlist_entries` row it holds. `playlist_entries` carries no
    /// foreign key (nothing cascades to it), so the entries must be deleted
    /// explicitly, in the same transaction, before the playlist itself.
    async fn delete_playlist(&self, uuid: Uuid) -> Result<(), DomainError>;

    /// Append `file_uuids` to the playlist identified by `playlist_uuid`, in
    /// order, at consecutive positions starting after whatever the playlist
    /// already holds, and return the new entries.
    ///
    /// All-or-nothing, in one transaction: every uuid must resolve to an
    /// audio file, or nothing is added. "Add this whole album" is one call
    /// so a rejected track cannot leave the rest of the album behind — the
    /// reason the caller passes a slice rather than looping one uuid at a
    /// time. `NotFound` when `playlist_uuid` or any `file_uuids` entry does
    /// not resolve to a file; `InvalidInput` when a resolved file is not
    /// `FileType::Audio` (a playlist holds audio only — video and documents
    /// have their own watchlists/reading lists).
    ///
    /// Deliberately not idempotent, unlike `ReadingListRepository::add_item`:
    /// `playlist_entries` carries no `UNIQUE (playlist_id, file_id)` (a set
    /// may open and close with the same song), so adding an already-present
    /// track appends a second entry rather than returning the existing one.
    async fn add_entries(
        &self,
        playlist_uuid: Uuid,
        file_uuids: &[Uuid],
    ) -> Result<Vec<PlaylistEntry>, DomainError>;

    /// Every entry the playlist identified by `playlist_uuid` holds, ordered
    /// by `position`.
    async fn list_entries(&self, playlist_uuid: Uuid) -> Result<Vec<PlaylistEntry>, DomainError>;

    /// Remove the entry identified by `entry_id` from the playlist
    /// identified by `playlist_uuid`, then renumber the remaining entries so
    /// `position` stays contiguous `0..n-1`. Delete and renumber happen in
    /// one transaction: a failure between them would leave a gap, and every
    /// later position calculation (`add_entries`'s `next_position`,
    /// reordering) assumes contiguity.
    ///
    /// `entry_id` is global, not scoped to a playlist (`playlist_entries`
    /// carries no compound key), so this confirms the entry belongs to
    /// `playlist_uuid` before touching it -- otherwise one playlist could
    /// delete another's row. `NotFound` when `playlist_uuid` does not
    /// resolve, or when `entry_id` does not resolve to a row inside it.
    async fn remove_entry(&self, playlist_uuid: Uuid, entry_id: i64) -> Result<(), DomainError>;

    /// Move the entry identified by `entry_id` to `to_index` within the
    /// playlist identified by `playlist_uuid`, and return the playlist's
    /// full new order.
    ///
    /// The contract is deliberately "put entry X at index N", computed and
    /// renumbered here in one transaction -- never a list of positions the
    /// caller believes are correct. A caller sending its own arithmetic
    /// would be a second implementation of the ordering rule, and the two
    /// would drift (design, Risks; BR-02).
    ///
    /// Implemented as read-move-rewrite rather than shifting only the
    /// affected span: entries are read in position order, the target
    /// element is moved within an in-memory `Vec`, then every position is
    /// written back. Index arithmetic that shifts only the span between the
    /// old and new index -- computing which positions move up or down by
    /// one, in SQL, without ever materializing the full order -- is exactly
    /// where off-by-one errors live; moving an entry to its own current
    /// index must be a no-op, which that kind of span-shifting arithmetic
    /// can easily land one off from.
    ///
    /// `NotFound` when `playlist_uuid` does not resolve, or when `entry_id`
    /// does not resolve to a row inside it (entry ids are global, so this
    /// confirms membership the same way `remove_entry` does).
    /// `InvalidInput` when `to_index` is negative or `>=` the playlist's
    /// entry count -- there is no position past the end or before the start
    /// to move into.
    async fn move_entry(
        &self,
        playlist_uuid: Uuid,
        entry_id: i64,
        to_index: i64,
    ) -> Result<Vec<PlaylistEntry>, DomainError>;

    /// Every entry the playlist identified by `playlist_uuid` holds, in
    /// position order, each resolved to the full `FileView` shape every
    /// other listing answers (`catalog::queries::browse`) plus a `missing`
    /// flag (design section 5).
    ///
    /// Batched like `CatalogRepository::list_filtered_view`: one query for
    /// the entries plus their files, then one further query for audio
    /// metadata chunked at `MAX_SQLITE_PARAMS`, regardless of how many
    /// tracks the playlist holds -- never one query per track. Only the
    /// audio subtype needs batching (unlike the catalog's five-way fan-out)
    /// because `add_entries` accepts nothing but `FileType::Audio`.
    ///
    /// An entry whose file has since gone missing on disk (`missing_at`
    /// set) is still returned, with `missing: true` -- dropping it would
    /// delete curation work invisibly and make an unplugged drive look
    /// like an empty playlist rather than a broken one. Returns an empty
    /// `Vec` when `playlist_uuid` does not resolve; the caller (the browse
    /// handler) is responsible for the `NotFound` check via `find_by_uuid`,
    /// the same division of responsibility `list_entries` already has.
    async fn list_view(&self, playlist_uuid: Uuid) -> Result<Vec<PlaylistTrack>, DomainError>;
}

#[derive(Clone)]
pub struct SqlitePlaylistRepository {
    pool: SqlitePool,
}

impl SqlitePlaylistRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

impl PlaylistRepository for SqlitePlaylistRepository {
    async fn insert_playlist(&self, new_playlist: NewPlaylist) -> Result<Playlist, DomainError> {
        sqlx::query("INSERT INTO playlists (uuid, name) VALUES (?, ?)")
            .bind(new_playlist.uuid.to_string())
            .bind(&new_playlist.name)
            .execute(&self.pool)
            .await?;

        Ok(Playlist {
            uuid: new_playlist.uuid,
            name: new_playlist.name,
        })
    }

    async fn find_by_uuid(&self, uuid: Uuid) -> Result<Option<Playlist>, DomainError> {
        let row = sqlx::query("SELECT uuid, name FROM playlists WHERE uuid = ?")
            .bind(uuid.to_string())
            .fetch_optional(&self.pool)
            .await?;

        Ok(match row {
            Some(row) => {
                let uuid: String = row.try_get("uuid")?;
                let name: String = row.try_get("name")?;
                Some(Playlist {
                    uuid: Uuid::parse_str(&uuid).map_err(|err| {
                        DomainError::internal(format!("corrupt playlist uuid: {err}"))
                    })?,
                    name,
                })
            }
            None => None,
        })
    }

    async fn list_all(&self) -> Result<Vec<Playlist>, DomainError> {
        let rows = sqlx::query("SELECT uuid, name FROM playlists ORDER BY name")
            .fetch_all(&self.pool)
            .await?;

        rows.into_iter()
            .map(|row| {
                let uuid: String = row.try_get("uuid")?;
                let name: String = row.try_get("name")?;
                Ok(Playlist {
                    uuid: Uuid::parse_str(&uuid).map_err(|err| {
                        DomainError::internal(format!("corrupt playlist uuid: {err}"))
                    })?,
                    name,
                })
            })
            .collect()
    }

    async fn rename_playlist(&self, uuid: Uuid, name: String) -> Result<Playlist, DomainError> {
        let affected = sqlx::query("UPDATE playlists SET name = ? WHERE uuid = ?")
            .bind(&name)
            .bind(uuid.to_string())
            .execute(&self.pool)
            .await?
            .rows_affected();
        if affected == 0 {
            return Err(DomainError::NotFound);
        }

        Ok(Playlist { uuid, name })
    }

    async fn delete_playlist(&self, uuid: Uuid) -> Result<(), DomainError> {
        let mut tx = self.pool.begin_with(WRITE_TX).await?;

        // Delete every `playlist_entries` row the playlist holds before
        // removing the playlist itself -- a deleted playlist must not leave
        // orphaned `playlist_entries` rows (nothing cascades to them; see
        // the migration's comment). The referenced files themselves are
        // untouched.
        sqlx::query(
            "DELETE FROM playlist_entries \
             WHERE playlist_id = (SELECT id FROM playlists WHERE uuid = ?)",
        )
        .bind(uuid.to_string())
        .execute(&mut *tx)
        .await?;

        sqlx::query("DELETE FROM playlists WHERE uuid = ?")
            .bind(uuid.to_string())
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(())
    }

    async fn add_entries(
        &self,
        playlist_uuid: Uuid,
        file_uuids: &[Uuid],
    ) -> Result<Vec<PlaylistEntry>, DomainError> {
        let mut tx = self.pool.begin_with(WRITE_TX).await?;

        let playlist_id: i64 = sqlx::query("SELECT id FROM playlists WHERE uuid = ?")
            .bind(playlist_uuid.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(DomainError::NotFound)?
            .try_get("id")?;

        // Resolve every uuid to a `files.id` and confirm it is audio before
        // inserting anything -- a uuid that fails partway must not leave
        // the earlier ones in the slice added (the whole reason this takes
        // a slice rather than being called once per uuid).
        let mut resolved: Vec<i64> = Vec::with_capacity(file_uuids.len());
        for file_uuid in file_uuids {
            let row = sqlx::query("SELECT id, type FROM files WHERE uuid = ?")
                .bind(file_uuid.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(DomainError::NotFound)?;

            let file_id: i64 = row.try_get("id")?;
            let file_type: String = row.try_get("type")?;
            if file_type != FileType::Audio.as_str() {
                return Err(DomainError::InvalidInput(format!(
                    "file {file_uuid} is not audio"
                )));
            }
            resolved.push(file_id);
        }

        let next_position: i64 = sqlx::query(
            "SELECT COALESCE(MAX(position), -1) + 1 AS next_position \
             FROM playlist_entries WHERE playlist_id = ?",
        )
        .bind(playlist_id)
        .fetch_one(&mut *tx)
        .await?
        .try_get("next_position")?;

        let mut entries = Vec::with_capacity(file_uuids.len());
        for (offset, (file_uuid, file_id)) in file_uuids.iter().zip(resolved).enumerate() {
            let position = next_position + offset as i64;
            let id = sqlx::query(
                "INSERT INTO playlist_entries (playlist_id, file_id, position) \
                 VALUES (?, ?, ?)",
            )
            .bind(playlist_id)
            .bind(file_id)
            .bind(position)
            .execute(&mut *tx)
            .await?
            .last_insert_rowid();

            entries.push(PlaylistEntry {
                id,
                file_uuid: *file_uuid,
                position,
            });
        }

        tx.commit().await?;
        Ok(entries)
    }

    async fn list_entries(&self, playlist_uuid: Uuid) -> Result<Vec<PlaylistEntry>, DomainError> {
        let rows = sqlx::query(
            "SELECT pe.id, f.uuid AS file_uuid, pe.position \
             FROM playlist_entries pe \
             JOIN files f ON f.id = pe.file_id \
             WHERE pe.playlist_id = (SELECT id FROM playlists WHERE uuid = ?) \
             ORDER BY pe.position",
        )
        .bind(playlist_uuid.to_string())
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                let id: i64 = row.try_get("id")?;
                let file_uuid: String = row.try_get("file_uuid")?;
                let position: i64 = row.try_get("position")?;
                Ok(PlaylistEntry {
                    id,
                    file_uuid: Uuid::parse_str(&file_uuid).map_err(|err| {
                        DomainError::internal(format!("corrupt playlist entry file uuid: {err}"))
                    })?,
                    position,
                })
            })
            .collect()
    }

    async fn remove_entry(&self, playlist_uuid: Uuid, entry_id: i64) -> Result<(), DomainError> {
        let mut tx = self.pool.begin_with(WRITE_TX).await?;

        let playlist_id: i64 = sqlx::query("SELECT id FROM playlists WHERE uuid = ?")
            .bind(playlist_uuid.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(DomainError::NotFound)?
            .try_get("id")?;

        // Confirm the entry belongs to this playlist before deleting it --
        // entry ids are global, so without this check one playlist could
        // delete another's row.
        let position: i64 =
            sqlx::query("SELECT position FROM playlist_entries WHERE id = ? AND playlist_id = ?")
                .bind(entry_id)
                .bind(playlist_id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(DomainError::NotFound)?
                .try_get("position")?;

        sqlx::query("DELETE FROM playlist_entries WHERE id = ?")
            .bind(entry_id)
            .execute(&mut *tx)
            .await?;

        // Close the gap the removed entry left so positions stay contiguous
        // `0..n-1` -- everything downstream of it, including this repo's own
        // `add_entries` `next_position` calculation, assumes that.
        sqlx::query(
            "UPDATE playlist_entries SET position = position - 1 \
             WHERE playlist_id = ? AND position > ?",
        )
        .bind(playlist_id)
        .bind(position)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    async fn move_entry(
        &self,
        playlist_uuid: Uuid,
        entry_id: i64,
        to_index: i64,
    ) -> Result<Vec<PlaylistEntry>, DomainError> {
        let mut tx = self.pool.begin_with(WRITE_TX).await?;

        let playlist_id: i64 = sqlx::query("SELECT id FROM playlists WHERE uuid = ?")
            .bind(playlist_uuid.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(DomainError::NotFound)?
            .try_get("id")?;

        // Read every entry in position order, inside the same transaction
        // as the write below, so the move is computed against a consistent
        // snapshot.
        let rows = sqlx::query(
            "SELECT pe.id, f.uuid AS file_uuid, pe.position \
             FROM playlist_entries pe \
             JOIN files f ON f.id = pe.file_id \
             WHERE pe.playlist_id = ? \
             ORDER BY pe.position",
        )
        .bind(playlist_id)
        .fetch_all(&mut *tx)
        .await?;

        let mut entries: Vec<(i64, Uuid)> = Vec::with_capacity(rows.len());
        for row in rows {
            let id: i64 = row.try_get("id")?;
            let file_uuid: String = row.try_get("file_uuid")?;
            entries.push((
                id,
                Uuid::parse_str(&file_uuid).map_err(|err| {
                    DomainError::internal(format!("corrupt playlist entry file uuid: {err}"))
                })?,
            ));
        }

        // `entry_id` is global, not scoped to a playlist, so confirm it
        // belongs to this one before moving anything -- otherwise one
        // playlist could reorder using another's row id.
        let from_index = entries
            .iter()
            .position(|(id, _)| *id == entry_id)
            .ok_or(DomainError::NotFound)?;

        if to_index < 0 || to_index as usize >= entries.len() {
            return Err(DomainError::InvalidInput(format!(
                "to_index {to_index} is out of range for a playlist of {} entries",
                entries.len()
            )));
        }
        let to_index = to_index as usize;

        // Move the element within an in-memory Vec rather than computing
        // which positions shift by how much -- simple and obviously
        // correct, including the "moved to where it already is" case,
        // which stays a no-op here without a special case.
        let moved = entries.remove(from_index);
        entries.insert(to_index, moved);

        for (position, (id, _)) in entries.iter().enumerate() {
            sqlx::query("UPDATE playlist_entries SET position = ? WHERE id = ?")
                .bind(position as i64)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }

        tx.commit().await?;

        Ok(entries
            .into_iter()
            .enumerate()
            .map(|(position, (id, file_uuid))| PlaylistEntry {
                id,
                file_uuid,
                position: position as i64,
            })
            .collect())
    }

    async fn list_view(&self, playlist_uuid: Uuid) -> Result<Vec<PlaylistTrack>, DomainError> {
        // Query 1: entries joined to their files, in position order. A
        // plain INNER JOIN against `files` is safe here -- a row leaves
        // `files` only on a hard purge (UC-09), never merely by going
        // missing (`missing_at` is a column on the still-present row) or by
        // soft-delete (`state`, likewise still present). Joining against a
        // *state-filtered* view instead would silently drop exactly the
        // entries design section 5 says must be kept and flagged.
        let rows = sqlx::query(
            "SELECT pe.id, pe.position, f.id AS file_id, f.uuid, f.path, f.name, f.type, \
             f.content_hash, f.state, f.deleted_at, f.indexed_at, f.missing_at, f.size_bytes, \
             f.mtime \
             FROM playlist_entries pe \
             JOIN files f ON f.id = pe.file_id \
             WHERE pe.playlist_id = (SELECT id FROM playlists WHERE uuid = ?) \
             ORDER BY pe.position",
        )
        .bind(playlist_uuid.to_string())
        .fetch_all(&self.pool)
        .await?;

        struct EntryFile {
            entry_id: i64,
            position: i64,
            file_id: i64,
            file: File,
        }

        let mut entries = Vec::with_capacity(rows.len());
        let mut file_ids = Vec::with_capacity(rows.len());
        for row in rows {
            let entry_id: i64 = row.try_get("id")?;
            let position: i64 = row.try_get("position")?;
            let file_id: i64 = row.try_get("file_id")?;
            let file = parse_playlist_file_row(&row)?;
            file_ids.push(file_id);
            entries.push(EntryFile {
                entry_id,
                position,
                file_id,
                file,
            });
        }

        // Query 2 (chunked): audio metadata for every distinct file, keyed
        // by internal id so a track appearing twice in the playlist
        // resolves the file once and both entries attach to the same
        // fetched row -- never a second query for the repeat. Deduped here
        // so a repeated track doesn't inflate the chunk count or bind the
        // same id twice in one `IN (...)` list -- `file_ids` above is
        // pushed one-per-entry, so without this a playlist holding the same
        // track many times would pad the batch with duplicate parameters
        // for no benefit.
        let mut distinct_file_ids = file_ids.clone();
        distinct_file_ids.sort_unstable();
        distinct_file_ids.dedup();
        let audio = self.batch_audio_metadata(&distinct_file_ids).await?;

        Ok(entries
            .into_iter()
            .map(|entry| {
                let missing = entry.file.missing_at.is_some();
                PlaylistTrack {
                    entry_id: entry.entry_id,
                    position: entry.position,
                    missing,
                    file: FileView {
                        metadata: audio.get(&entry.file_id).cloned(),
                        file: entry.file,
                        width: None,
                        height: None,
                        page_count: None,
                        duration_seconds: None,
                        comic_page_count: None,
                    },
                }
            })
            .collect())
    }
}

impl SqlitePlaylistRepository {
    /// Batch-fetch audio metadata for every id in `ids`, chunked at
    /// `MAX_SQLITE_PARAMS` -- the same technique
    /// `CatalogRepository::list_filtered_view`'s `batch_audio` uses, kept
    /// as its own copy here because a playlist only ever needs the one
    /// subtype (`add_entries` rejects everything but audio).
    async fn batch_audio_metadata(
        &self,
        ids: &[i64],
    ) -> Result<HashMap<i64, SubtypeMetadata>, DomainError> {
        let mut out = HashMap::new();
        for chunk in ids.chunks(MAX_SQLITE_PARAMS) {
            if chunk.is_empty() {
                continue;
            }
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
            let sql = format!(
                "SELECT file_id, title, artist, album, year, genre, track, album_artist \
                 FROM audio_files WHERE file_id IN ({placeholders})"
            );
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                query = query.bind(id);
            }
            let rows = query.fetch_all(&self.pool).await?;
            for row in rows {
                let file_id: i64 = row.try_get("file_id")?;
                let title: Option<String> = row.try_get("title")?;
                let artist: Option<String> = row.try_get("artist")?;
                let album: Option<String> = row.try_get("album")?;
                let year: Option<i64> = row.try_get("year")?;
                let genre: Option<String> = row.try_get("genre")?;
                let track: Option<i64> = row.try_get("track")?;
                let album_artist: Option<String> = row.try_get("album_artist")?;
                let all_none = title.is_none()
                    && artist.is_none()
                    && album.is_none()
                    && year.is_none()
                    && genre.is_none()
                    && track.is_none()
                    && album_artist.is_none();
                if !all_none {
                    out.insert(
                        file_id,
                        SubtypeMetadata::Audio {
                            title,
                            artist,
                            album,
                            year,
                            genre,
                            track,
                            album_artist,
                        },
                    );
                }
            }
        }
        Ok(out)
    }
}

/// Parse the file half of `list_view`'s query-1 row into a `File`. A
/// smaller copy of `catalog::repos::parse_file_row`, which is private to
/// its own module and keyed off a positional tuple rather than named
/// columns -- this reads by column name since the query above interleaves
/// entry and file columns.
fn parse_playlist_file_row(row: &sqlx::sqlite::SqliteRow) -> Result<File, DomainError> {
    let uuid: String = row.try_get("uuid")?;
    let path: String = row.try_get("path")?;
    let name: String = row.try_get("name")?;
    let type_str: String = row.try_get("type")?;
    let content_hash: Option<String> = row.try_get("content_hash")?;
    let state_str: String = row.try_get("state")?;
    let deleted_at: Option<String> = row.try_get("deleted_at")?;
    let indexed_at: String = row.try_get("indexed_at")?;
    let missing_at: Option<String> = row.try_get("missing_at")?;
    let size_bytes: Option<i64> = row.try_get("size_bytes")?;
    let mtime: Option<String> = row.try_get("mtime")?;

    let file_type = FileType::from_wire(&type_str).ok_or_else(|| {
        DomainError::internal(format!("corrupt playlist entry file type: {type_str}"))
    })?;
    let state = match state_str.as_str() {
        "active" => FileState::Active,
        "deleted" => FileState::Deleted,
        other => {
            return Err(DomainError::internal(format!(
                "corrupt playlist entry file state: {other}"
            )))
        }
    };

    Ok(File {
        uuid: Uuid::parse_str(&uuid).map_err(|err| {
            DomainError::internal(format!("corrupt playlist entry file uuid: {err}"))
        })?,
        path,
        name,
        file_type,
        content_hash,
        size_bytes,
        mtime: mtime
            .map(|s| parse_playlist_timestamp(&s, "mtime"))
            .transpose()?,
        state,
        deleted_at: deleted_at
            .map(|s| parse_playlist_timestamp(&s, "deleted_at"))
            .transpose()?,
        indexed_at: parse_playlist_timestamp(&indexed_at, "indexed_at")?,
        missing_at: missing_at
            .map(|s| parse_playlist_timestamp(&s, "missing_at"))
            .transpose()?,
    })
}

fn parse_playlist_timestamp(value: &str, column: &str) -> Result<DateTime<Utc>, DomainError> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|_| {
            DomainError::internal(format!(
                "corrupt playlist entry row: unparseable {column} {value:?}"
            ))
        })
}
