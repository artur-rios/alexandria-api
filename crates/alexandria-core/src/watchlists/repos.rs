use sqlx::sqlite::SqlitePool;

use crate::errors::DomainError;
use crate::watchlists::model::{NewWatchlist, Watchlist};

/// Watchlists repository port. The create handler depends on this trait so
/// its decision logic (validation, uuid minting) is unit-tested against an
/// in-memory fake with no database (Testing Specification §6.2). The Sqlite
/// implementation persists `watchlists` rows.
///
/// UC-21..25 add their own methods when they ship.
#[allow(async_fn_in_trait)]
pub trait WatchlistRepository: Send + Sync {
    /// Persist a new watchlist and return the stored record (UC-20 /
    /// FR-WL-01). The caller has already validated the name.
    async fn insert_watchlist(&self, new_watchlist: NewWatchlist)
        -> Result<Watchlist, DomainError>;
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
}
