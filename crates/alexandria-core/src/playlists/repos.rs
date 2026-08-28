use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use uuid::Uuid;

use crate::catalog::model::FileType;
use crate::errors::{DomainError, WRITE_TX};
use crate::playlists::model::{NewPlaylist, Playlist, PlaylistEntry};

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
}
