use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use uuid::Uuid;

use crate::collections::model::{Collection, CollectionKind, CollectionSummary, NewCollection};
use crate::errors::{DomainError, WRITE_TX};

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

    /// List the collections, optionally narrowed to one `kind`, each with the
    /// number of items it currently holds (UC-46 / FR-CO-08).
    ///
    /// `None` for `kind` is every collection. An empty result is a state and
    /// not an error (AF-01), so this returns an empty `Vec` rather than
    /// `Option`.
    async fn list_collections(
        &self,
        kind: Option<CollectionKind>,
    ) -> Result<Vec<CollectionSummary>, DomainError>;
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

    async fn list_collections(
        &self,
        kind: Option<CollectionKind>,
    ) -> Result<Vec<CollectionSummary>, DomainError> {
        // The count is a correlated subquery per table rather than a join with
        // a GROUP BY: a collection's `kind` fixes which table can hold its
        // members, and summing both keeps the count right for a row whose
        // `kind` was written before that invariant was enforced — the same
        // reason `delete_collection` clears both.
        //
        // `state = 'active'` on each: UC-14's listing excludes soft-deleted
        // members, and a count that included them would disagree with the list
        // it describes.
        let mut sql = String::from(
            "SELECT c.uuid, c.name, c.kind,              (SELECT COUNT(*) FROM files f                 WHERE f.collection_id = c.id AND f.state = 'active')              + (SELECT COUNT(*) FROM bookmarks b                 WHERE b.collection_id = c.id AND b.state = 'active') AS item_count              FROM collections c",
        );
        if kind.is_some() {
            sql.push_str(" WHERE c.kind = ?");
        }
        // Ordered by name so the listing is stable between calls; the caller
        // presents it as it arrives.
        sql.push_str(" ORDER BY c.name COLLATE NOCASE, c.uuid");

        // sqlx 0.9 refuses a runtime-built SQL string unless the caller
        // asserts it was audited. `sql` is assembled only from string literals
        // chosen by the `Option<CollectionKind>` parameter above — no caller
        // input reaches it, and the kind itself is still a bound `?`
        // parameter.
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
        if let Some(kind) = kind {
            query = query.bind(kind.as_str());
        }

        let rows = query.fetch_all(&self.pool).await?;

        let mut summaries = Vec::with_capacity(rows.len());
        for row in rows {
            let uuid: String = row.try_get("uuid")?;
            let name: String = row.try_get("name")?;
            let kind: String = row.try_get("kind")?;
            let item_count: i64 = row.try_get("item_count")?;

            summaries.push(CollectionSummary {
                uuid: Uuid::parse_str(&uuid).map_err(|err| {
                    DomainError::internal(format!("corrupt collection uuid: {err}"))
                })?,
                name,
                kind: CollectionKind::parse(&kind).ok_or_else(|| {
                    DomainError::internal(format!("corrupt collection kind: {kind}"))
                })?,
                item_count,
            });
        }

        Ok(summaries)
    }

    async fn delete_collection(&self, uuid: Uuid) -> Result<(), DomainError> {
        let mut tx = self.pool.begin_with(WRITE_TX).await?;

        // Unlink every item the collection holds before removing it — a
        // deleted collection must not leave a `collection_id` pointing at a
        // row that no longer exists (UC-12 / FR-CO-04). A collection's `kind`
        // fixes which table actually holds members, but both are cleared
        // unconditionally: the statement for the other table matches nothing,
        // and running both keeps the unlink correct even for a row whose
        // `kind` was written before the invariant was enforced.
        sqlx::query(
            "UPDATE files SET collection_id = NULL \
             WHERE collection_id = (SELECT id FROM collections WHERE uuid = ?)",
        )
        .bind(uuid.to_string())
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "UPDATE bookmarks SET collection_id = NULL \
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
