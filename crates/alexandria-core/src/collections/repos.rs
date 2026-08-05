use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use uuid::Uuid;

use crate::collections::model::{Collection, CollectionKind, NewCollection};
use crate::errors::DomainError;

/// Collections repository port. The create handler depends on this trait so
/// its decision logic (validation, uuid minting) is unit-tested against an
/// in-memory fake with no database (Testing Specification §6.2). The Sqlite
/// implementation persists `collections` rows.
///
/// Item management (UC-13/14) adds its own methods when it ships.
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

    /// Look a collection up by its public uuid (UC-11 AF-02, UC-12 AF-01).
    /// `None` when no such collection exists.
    async fn find_by_uuid(&self, uuid: Uuid) -> Result<Option<Collection>, DomainError>;

    /// Rename a collection and return the updated record (UC-11 / FR-CO-03).
    /// The caller has already confirmed the collection exists and validated
    /// the new name.
    async fn rename_collection(&self, uuid: Uuid, name: String) -> Result<Collection, DomainError>;

    /// Delete a collection, unlinking (not deleting) every item it contains
    /// (UC-12 / FR-CO-04). The caller has already confirmed the collection
    /// exists.
    async fn delete_collection(&self, uuid: Uuid) -> Result<(), DomainError>;
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

    async fn find_by_uuid(&self, uuid: Uuid) -> Result<Option<Collection>, DomainError> {
        let row = sqlx::query("SELECT uuid, name, kind FROM collections WHERE uuid = ?")
            .bind(uuid.to_string())
            .fetch_optional(&self.pool)
            .await?;

        Ok(match row {
            Some(row) => {
                let uuid: String = row.try_get("uuid")?;
                let name: String = row.try_get("name")?;
                let kind: String = row.try_get("kind")?;
                Some(Collection {
                    uuid: Uuid::parse_str(&uuid).map_err(|err| {
                        DomainError::internal(format!("corrupt collection uuid: {err}"))
                    })?,
                    name,
                    kind: CollectionKind::parse(&kind).ok_or_else(|| {
                        DomainError::internal(format!("corrupt collection kind: {kind}"))
                    })?,
                })
            }
            None => None,
        })
    }

    async fn rename_collection(&self, uuid: Uuid, name: String) -> Result<Collection, DomainError> {
        sqlx::query("UPDATE collections SET name = ? WHERE uuid = ?")
            .bind(&name)
            .bind(uuid.to_string())
            .execute(&self.pool)
            .await?;

        self.find_by_uuid(uuid).await?.ok_or(DomainError::NotFound)
    }

    async fn delete_collection(&self, uuid: Uuid) -> Result<(), DomainError> {
        let mut tx = self.pool.begin().await?;

        // Unlink every file the collection holds before removing it — a
        // deleted collection must not leave `files.collection_id` pointing at
        // a row that no longer exists (UC-12 / FR-CO-04). Bookmarks get the
        // same treatment once UC-15 introduces a `bookmarks` table; there is
        // nothing to unlink there yet.
        sqlx::query(
            "UPDATE files SET collection_id = NULL \
             WHERE collection_id = (SELECT id FROM collections WHERE uuid = ?)",
        )
        .bind(uuid.to_string())
        .execute(&mut *tx)
        .await?;

        sqlx::query("DELETE FROM collections WHERE uuid = ?")
            .bind(uuid.to_string())
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(())
    }
}
