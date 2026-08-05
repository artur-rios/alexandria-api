use sqlx::sqlite::SqlitePool;

use crate::errors::DomainError;
use crate::reading_lists::model::{NewReadingList, ReadingList};

/// Reading lists repository port. The create handler depends on this trait
/// so its decision logic (validation, uuid minting) is unit-tested against
/// an in-memory fake with no database (Testing Specification §6.2). The
/// Sqlite implementation persists `reading_lists` rows.
///
/// UC-27..31 add their own methods when they ship.
#[allow(async_fn_in_trait)]
pub trait ReadingListRepository: Send + Sync {
    /// Persist a new reading list and return the stored record (UC-26 /
    /// FR-RL-01). The caller has already validated the name.
    async fn insert_reading_list(
        &self,
        new_reading_list: NewReadingList,
    ) -> Result<ReadingList, DomainError>;
}

#[derive(Clone)]
pub struct SqliteReadingListRepository {
    pool: SqlitePool,
}

impl SqliteReadingListRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

impl ReadingListRepository for SqliteReadingListRepository {
    async fn insert_reading_list(
        &self,
        new_reading_list: NewReadingList,
    ) -> Result<ReadingList, DomainError> {
        sqlx::query("INSERT INTO reading_lists (uuid, name) VALUES (?, ?)")
            .bind(new_reading_list.uuid.to_string())
            .bind(&new_reading_list.name)
            .execute(&self.pool)
            .await?;

        Ok(ReadingList {
            uuid: new_reading_list.uuid,
            name: new_reading_list.name,
        })
    }
}
