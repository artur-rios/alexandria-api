use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use uuid::Uuid;

use crate::errors::DomainError;
use crate::watchlists::model::{NewWatchlist, WatchProgress, WatchState, Watchlist};

/// Watchlists repository port. The create handler depends on this trait so
/// its decision logic (validation, uuid minting) is unit-tested against an
/// in-memory fake with no database (Testing Specification §6.2). The Sqlite
/// implementation persists `watchlists` and `watch_progress` rows.
///
/// UC-23..25 add their own methods when they ship.
#[allow(async_fn_in_trait)]
pub trait WatchlistRepository: Send + Sync {
    /// Persist a new watchlist and return the stored record (UC-20 /
    /// FR-WL-01). The caller has already validated the name.
    async fn insert_watchlist(&self, new_watchlist: NewWatchlist)
        -> Result<Watchlist, DomainError>;

    /// Look a watchlist up by its public uuid (UC-22 AF-02). `None` when no
    /// such watchlist exists.
    async fn find_by_uuid(&self, uuid: Uuid) -> Result<Option<Watchlist>, DomainError>;

    /// Link the video identified by `video_uuid` to the watchlist identified
    /// by `watchlist_uuid`, creating a `Pending` WatchProgress (UC-22 /
    /// FR-WL-02), and return it. Idempotent: if the pair is already linked,
    /// the existing WatchProgress is returned unchanged rather than reset to
    /// `Pending` — UC-23 may have already advanced it. The caller has
    /// already confirmed both exist and that the video is a `VideoFile`.
    async fn add_video(
        &self,
        watchlist_uuid: Uuid,
        video_uuid: Uuid,
    ) -> Result<WatchProgress, DomainError>;
}

#[derive(Clone)]
pub struct SqliteWatchlistRepository {
    pool: SqlitePool,
}

impl SqliteWatchlistRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

impl WatchlistRepository for SqliteWatchlistRepository {
    async fn insert_watchlist(
        &self,
        new_watchlist: NewWatchlist,
    ) -> Result<Watchlist, DomainError> {
        sqlx::query("INSERT INTO watchlists (uuid, name) VALUES (?, ?)")
            .bind(new_watchlist.uuid.to_string())
            .bind(&new_watchlist.name)
            .execute(&self.pool)
            .await?;

        Ok(Watchlist {
            uuid: new_watchlist.uuid,
            name: new_watchlist.name,
        })
    }

    async fn find_by_uuid(&self, uuid: Uuid) -> Result<Option<Watchlist>, DomainError> {
        let row = sqlx::query("SELECT uuid, name FROM watchlists WHERE uuid = ?")
            .bind(uuid.to_string())
            .fetch_optional(&self.pool)
            .await?;

        Ok(match row {
            Some(row) => {
                let uuid: String = row.try_get("uuid")?;
                let name: String = row.try_get("name")?;
                Some(Watchlist {
                    uuid: Uuid::parse_str(&uuid).map_err(|err| {
                        DomainError::internal(format!("corrupt watchlist uuid: {err}"))
                    })?,
                    name,
                })
            }
            None => None,
        })
    }

    async fn add_video(
        &self,
        watchlist_uuid: Uuid,
        video_uuid: Uuid,
    ) -> Result<WatchProgress, DomainError> {
        sqlx::query(
            "INSERT INTO watch_progress (watchlist_id, video_file_id, state) \
             VALUES ( \
                (SELECT id FROM watchlists WHERE uuid = ?), \
                (SELECT id FROM files WHERE uuid = ?), \
                'pending' \
             ) \
             ON CONFLICT (watchlist_id, video_file_id) DO NOTHING",
        )
        .bind(watchlist_uuid.to_string())
        .bind(video_uuid.to_string())
        .execute(&self.pool)
        .await?;

        let row = sqlx::query(
            "SELECT wp.state, wp.current_episode, wp.total_episodes \
             FROM watch_progress wp \
             JOIN watchlists w ON w.id = wp.watchlist_id \
             JOIN files f ON f.id = wp.video_file_id \
             WHERE w.uuid = ? AND f.uuid = ?",
        )
        .bind(watchlist_uuid.to_string())
        .bind(video_uuid.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(DomainError::NotFound)?;

        let state_str: String = row.try_get("state")?;
        let current_episode: Option<i64> = row.try_get("current_episode")?;
        let total_episodes: Option<i64> = row.try_get("total_episodes")?;

        Ok(WatchProgress {
            watchlist_uuid,
            video_uuid,
            state: WatchState::parse(&state_str).ok_or_else(|| {
                DomainError::internal(format!("corrupt watch_progress state: {state_str}"))
            })?,
            current_episode,
            total_episodes,
        })
    }
}
