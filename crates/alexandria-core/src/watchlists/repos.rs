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

    /// Every persisted watchlist, ordered by name (UC-21 / FR-WL-08).
    async fn list_all(&self) -> Result<Vec<Watchlist>, DomainError>;

    /// Every WatchProgress row for the watchlist identified by `watchlist_uuid`,
    /// ordered by video uuid (UC-21 / FR-WL-08).
    async fn list_progress(&self, watchlist_uuid: Uuid) -> Result<Vec<WatchProgress>, DomainError>;

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

    /// Look up the WatchProgress linking `video_uuid` to `watchlist_uuid`
    /// (UC-23 AF-02). `None` when the video is not on that watchlist.
    async fn find_progress(
        &self,
        watchlist_uuid: Uuid,
        video_uuid: Uuid,
    ) -> Result<Option<WatchProgress>, DomainError>;

    /// Replace the state and episode fields of the WatchProgress linking
    /// `video_uuid` to `watchlist_uuid` (UC-23 / FR-WL-04, FR-WL-05), and
    /// return the updated record. Full replace: `current_episode` and
    /// `total_episodes` are written as given, `None` writes `NULL`. The
    /// caller has already confirmed the WatchProgress exists and that the
    /// transition to `state` is valid.
    async fn update_progress(
        &self,
        watchlist_uuid: Uuid,
        video_uuid: Uuid,
        state: WatchState,
        current_episode: Option<i64>,
        total_episodes: Option<i64>,
    ) -> Result<WatchProgress, DomainError>;

    /// Delete the WatchProgress linking `video_uuid` to `watchlist_uuid`
    /// (UC-24 / FR-WL-06). The VideoFile itself is untouched. Returns
    /// `NotFound` when no such WatchProgress exists (AF-01).
    async fn remove_progress(
        &self,
        watchlist_uuid: Uuid,
        video_uuid: Uuid,
    ) -> Result<(), DomainError>;

    /// Delete the watchlist identified by `uuid`, including every
    /// WatchProgress entry it holds (UC-25 / FR-WL-07). The VideoFiles
    /// themselves are untouched — this deletes the tracking rows only.
    async fn delete_watchlist(&self, uuid: Uuid) -> Result<(), DomainError>;
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

    async fn list_all(&self) -> Result<Vec<Watchlist>, DomainError> {
        let rows = sqlx::query("SELECT uuid, name FROM watchlists ORDER BY name")
            .fetch_all(&self.pool)
            .await?;

        rows.into_iter()
            .map(|row| {
                let uuid: String = row.try_get("uuid")?;
                let name: String = row.try_get("name")?;
                Ok(Watchlist {
                    uuid: Uuid::parse_str(&uuid).map_err(|err| {
                        DomainError::internal(format!("corrupt watchlist uuid: {err}"))
                    })?,
                    name,
                })
            })
            .collect()
    }

    async fn list_progress(&self, watchlist_uuid: Uuid) -> Result<Vec<WatchProgress>, DomainError> {
        let rows = sqlx::query(
            "SELECT f.uuid AS video_uuid, wp.state, wp.current_episode, wp.total_episodes \
             FROM watch_progress wp \
             JOIN files f ON f.id = wp.video_file_id \
             WHERE wp.watchlist_id = (SELECT id FROM watchlists WHERE uuid = ?) \
             ORDER BY f.uuid",
        )
        .bind(watchlist_uuid.to_string())
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                let video_uuid: String = row.try_get("video_uuid")?;
                let state_str: String = row.try_get("state")?;
                let current_episode: Option<i64> = row.try_get("current_episode")?;
                let total_episodes: Option<i64> = row.try_get("total_episodes")?;
                Ok(WatchProgress {
                    watchlist_uuid,
                    video_uuid: Uuid::parse_str(&video_uuid).map_err(|err| {
                        DomainError::internal(format!("corrupt video uuid: {err}"))
                    })?,
                    state: WatchState::parse(&state_str).ok_or_else(|| {
                        DomainError::internal(format!("corrupt watch_progress state: {state_str}"))
                    })?,
                    current_episode,
                    total_episodes,
                })
            })
            .collect()
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

    async fn find_progress(
        &self,
        watchlist_uuid: Uuid,
        video_uuid: Uuid,
    ) -> Result<Option<WatchProgress>, DomainError> {
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
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        let state_str: String = row.try_get("state")?;
        let current_episode: Option<i64> = row.try_get("current_episode")?;
        let total_episodes: Option<i64> = row.try_get("total_episodes")?;

        Ok(Some(WatchProgress {
            watchlist_uuid,
            video_uuid,
            state: WatchState::parse(&state_str).ok_or_else(|| {
                DomainError::internal(format!("corrupt watch_progress state: {state_str}"))
            })?,
            current_episode,
            total_episodes,
        }))
    }

    async fn update_progress(
        &self,
        watchlist_uuid: Uuid,
        video_uuid: Uuid,
        state: WatchState,
        current_episode: Option<i64>,
        total_episodes: Option<i64>,
    ) -> Result<WatchProgress, DomainError> {
        sqlx::query(
            "UPDATE watch_progress \
             SET state = ?, current_episode = ?, total_episodes = ? \
             WHERE watchlist_id = (SELECT id FROM watchlists WHERE uuid = ?) \
               AND video_file_id = (SELECT id FROM files WHERE uuid = ?)",
        )
        .bind(state.as_str())
        .bind(current_episode)
        .bind(total_episodes)
        .bind(watchlist_uuid.to_string())
        .bind(video_uuid.to_string())
        .execute(&self.pool)
        .await?;

        Ok(WatchProgress {
            watchlist_uuid,
            video_uuid,
            state,
            current_episode,
            total_episodes,
        })
    }

    async fn remove_progress(
        &self,
        watchlist_uuid: Uuid,
        video_uuid: Uuid,
    ) -> Result<(), DomainError> {
        let affected = sqlx::query(
            "DELETE FROM watch_progress \
             WHERE watchlist_id = (SELECT id FROM watchlists WHERE uuid = ?) \
               AND video_file_id = (SELECT id FROM files WHERE uuid = ?)",
        )
        .bind(watchlist_uuid.to_string())
        .bind(video_uuid.to_string())
        .execute(&self.pool)
        .await?
        .rows_affected();
        if affected == 0 {
            return Err(DomainError::NotFound);
        }
        Ok(())
    }

    async fn delete_watchlist(&self, uuid: Uuid) -> Result<(), DomainError> {
        let mut tx = self.pool.begin().await?;

        // Delete every WatchProgress entry the watchlist holds before
        // removing it — a deleted watchlist must not leave orphaned
        // `watch_progress` rows (UC-25 / FR-WL-07). The VideoFiles
        // themselves are untouched.
        sqlx::query(
            "DELETE FROM watch_progress \
             WHERE watchlist_id = (SELECT id FROM watchlists WHERE uuid = ?)",
        )
        .bind(uuid.to_string())
        .execute(&mut *tx)
        .await?;

        sqlx::query("DELETE FROM watchlists WHERE uuid = ?")
            .bind(uuid.to_string())
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(())
    }
}
