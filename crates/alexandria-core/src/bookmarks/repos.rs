use sqlx::sqlite::SqlitePool;

use crate::bookmarks::model::{Bookmark, BookmarkState, NewBookmark};
use crate::errors::DomainError;

/// Bookmarks repository port. The create handler depends on this trait so its
/// decision logic (validation, uuid minting) is unit-tested against an
/// in-memory fake with no database (Testing Specification §6.2). The Sqlite
/// implementation persists `bookmarks` rows.
///
/// Only the write UC-15 needs lives here; UC-16..19 add their own methods
/// when they ship.
#[allow(async_fn_in_trait)]
pub trait BookmarkRepository: Send + Sync {
    /// Persist a new bookmark and return the stored record (UC-15 /
    /// FR-BM-01). The caller has already validated the url and title, and —
    /// when `collection_uuid` is `Some` — already confirmed that collection
    /// exists and is `kind = bookmark`.
    async fn insert_bookmark(&self, new_bookmark: NewBookmark) -> Result<Bookmark, DomainError>;
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
}
