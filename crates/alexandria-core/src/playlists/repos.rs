use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use uuid::Uuid;

use crate::errors::{DomainError, WRITE_TX};
use crate::playlists::model::{NewPlaylist, Playlist};

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
}
