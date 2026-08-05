use sqlx::sqlite::SqlitePool;

use crate::collections::model::{Collection, NewCollection};
use crate::errors::DomainError;

/// Collections repository port. The create handler depends on this trait so
/// its decision logic (validation, uuid minting) is unit-tested against an
/// in-memory fake with no database (Testing Specification §6.2). The Sqlite
/// implementation persists `collections` rows.
///
/// Only the write UC-10 needs lives here; renaming (UC-11), deletion (UC-12),
/// and item management (UC-13/14) add their own methods when they ship.
#[allow(async_fn_in_trait)]
pub trait CollectionRepository: Send + Sync {
    /// Persist a new collection and return the stored record (UC-10 /
    /// FR-CO-01, FR-CO-02). The caller has already validated the name; the
    /// `kind` is an enum, so the schema's CHECK constraint can only fail on a
    /// value this crate did not write.
    async fn insert_collection(
        &self,
        new_collection: NewCollection,
    ) -> Result<Collection, DomainError>;
}

#[derive(Clone)]
pub struct SqliteCollectionRepository {
    pool: SqlitePool,
}

impl SqliteCollectionRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

impl CollectionRepository for SqliteCollectionRepository {
    async fn insert_collection(
        &self,
        new_collection: NewCollection,
    ) -> Result<Collection, DomainError> {
        sqlx::query("INSERT INTO collections (uuid, name, kind) VALUES (?, ?, ?)")
            .bind(new_collection.uuid.to_string())
            .bind(&new_collection.name)
            .bind(new_collection.kind.as_str())
            .execute(&self.pool)
            .await?;

        Ok(Collection {
            uuid: new_collection.uuid,
            name: new_collection.name,
            kind: new_collection.kind,
        })
    }
}
