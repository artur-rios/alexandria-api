use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use uuid::Uuid;

use crate::bookmarks::model::{Bookmark, BookmarkState, NewBookmark};
use crate::errors::DomainError;

/// Bookmarks repository port. The create handler depends on this trait so its
/// decision logic (validation, uuid minting) is unit-tested against an
/// in-memory fake with no database (Testing Specification §6.2). The Sqlite
/// implementation persists `bookmarks` rows.
///
/// UC-16..19 add their own methods when they ship.
#[allow(async_fn_in_trait)]
pub trait BookmarkRepository: Send + Sync {
    /// Persist a new bookmark and return the stored record (UC-15 /
    /// FR-BM-01). The caller has already validated the url and title, and —
    /// when `collection_uuid` is `Some` — already confirmed that collection
    /// exists and is `kind = bookmark`.
    async fn insert_bookmark(&self, new_bookmark: NewBookmark) -> Result<Bookmark, DomainError>;

    /// Look a bookmark up by its public uuid (UC-13 AF-01/AF-02). `None` when
    /// no such bookmark exists.
    async fn find_by_uuid(&self, uuid: Uuid) -> Result<Option<Bookmark>, DomainError>;

    /// Link the bookmark identified by `uuid` to the collection identified by
    /// `collection_uuid` (UC-13 / FR-CO-05). The caller has already confirmed
    /// both exist and that the collection is `kind = bookmark`.
    async fn set_collection(&self, uuid: Uuid, collection_uuid: Uuid) -> Result<(), DomainError>;
}

#[derive(Clone)]
pub struct SqliteBookmarkRepository {
    pool: SqlitePool,
}

impl SqliteBookmarkRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

impl BookmarkRepository for SqliteBookmarkRepository {
    async fn insert_bookmark(&self, new_bookmark: NewBookmark) -> Result<Bookmark, DomainError> {
        // The collection's existence and kind were already confirmed by the
        // handler (UC-15 AF-02), so the id is resolved in the same statement
        // rather than re-read here — a `None` collection_uuid binds NULL.
        sqlx::query(
            "INSERT INTO bookmarks (uuid, url, title, collection_id) \
             VALUES (?, ?, ?, (SELECT id FROM collections WHERE uuid = ?))",
        )
        .bind(new_bookmark.uuid.to_string())
        .bind(&new_bookmark.url)
        .bind(&new_bookmark.title)
        .bind(new_bookmark.collection_uuid.map(|u| u.to_string()))
        .execute(&self.pool)
        .await?;

        Ok(Bookmark {
            uuid: new_bookmark.uuid,
            url: new_bookmark.url,
            title: new_bookmark.title,
            state: BookmarkState::Active,
            deleted_at: None,
            collection_uuid: new_bookmark.collection_uuid,
        })
    }

    async fn find_by_uuid(&self, uuid: Uuid) -> Result<Option<Bookmark>, DomainError> {
        let row = sqlx::query(
            "SELECT b.uuid, b.url, b.title, b.state, b.deleted_at, c.uuid AS collection_uuid \
             FROM bookmarks b LEFT JOIN collections c ON c.id = b.collection_id \
             WHERE b.uuid = ?",
        )
        .bind(uuid.to_string())
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        let uuid_str: String = row.try_get("uuid")?;
        let url: String = row.try_get("url")?;
        let title: String = row.try_get("title")?;
        let state_str: String = row.try_get("state")?;
        let deleted_at_str: Option<String> = row.try_get("deleted_at")?;
        let collection_uuid_str: Option<String> = row.try_get("collection_uuid")?;

        Ok(Some(Bookmark {
            uuid: Uuid::parse_str(&uuid_str)
                .map_err(|err| DomainError::internal(format!("corrupt bookmark uuid: {err}")))?,
            url,
            title,
            state: BookmarkState::parse(&state_str).ok_or_else(|| {
                DomainError::internal(format!("corrupt bookmark state: {state_str}"))
            })?,
            deleted_at: deleted_at_str
                .map(|s| {
                    chrono::DateTime::parse_from_rfc3339(&s)
                        .map(|dt| dt.with_timezone(&chrono::Utc))
                        .map_err(|err| {
                            DomainError::internal(format!("corrupt bookmark deleted_at: {err}"))
                        })
                })
                .transpose()?,
            collection_uuid: collection_uuid_str
                .map(|s| {
                    Uuid::parse_str(&s).map_err(|err| {
                        DomainError::internal(format!("corrupt bookmark collection_uuid: {err}"))
                    })
                })
                .transpose()?,
        }))
    }

    async fn set_collection(&self, uuid: Uuid, collection_uuid: Uuid) -> Result<(), DomainError> {
        let affected = sqlx::query(
            "UPDATE bookmarks SET collection_id = (SELECT id FROM collections WHERE uuid = ?) \
             WHERE uuid = ?",
        )
        .bind(collection_uuid.to_string())
        .bind(uuid.to_string())
        .execute(&self.pool)
        .await?
        .rows_affected();
        if affected == 0 {
            return Err(DomainError::NotFound);
        }
        Ok(())
    }
}
