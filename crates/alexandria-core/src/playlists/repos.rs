use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use uuid::Uuid;

use crate::errors::DomainError;
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
}
