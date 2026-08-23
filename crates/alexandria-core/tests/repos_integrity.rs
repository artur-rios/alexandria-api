//! Referential-integrity regressions at the repository layer, exercised
//! against a real migrated SQLite database.
//!
//! Foreign keys *are* enforced — sqlx sets `PRAGMA foreign_keys = ON` on every
//! connection — but only the subtype tables declare one. `watch_progress`,
//! `reading_progress`, and the two `collection_id` columns have no foreign key
//! at all (SQLite cannot add one via `ALTER TABLE`), so nothing cascades to
//! them and the repository performing a delete has to clear them by hand.
//! These tests pin the two places where it had not: UC-12's unlink of a
//! bookmark collection's members, and UC-08/UC-09's removal of the progress
//! rows that tracked a purged file.

use alexandria_core::bookmarks::model::NewBookmark;
use alexandria_core::bookmarks::repos::{BookmarkRepository, SqliteBookmarkRepository};
use alexandria_core::catalog::model::{FileType, NewFile};
use alexandria_core::catalog::repos::{CatalogRepository, SqliteCatalogRepository};
use alexandria_core::collections::model::{CollectionKind, NewCollection};
use alexandria_core::collections::repos::{CollectionRepository, SqliteCollectionRepository};
use alexandria_core::migrate::run_migrations;
use alexandria_core::reading_lists::model::{NewReadingList, ReadingTargetKind};
use alexandria_core::reading_lists::repos::{ReadingListRepository, SqliteReadingListRepository};
use alexandria_core::watchlists::model::NewWatchlist;
use alexandria_core::watchlists::repos::{SqliteWatchlistRepository, WatchlistRepository};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use uuid::Uuid;

async fn migrated_pool() -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");
    run_migrations(&pool).await.expect("migrate");
    pool
}

async fn insert_file(repo: &SqliteCatalogRepository, path: &str, file_type: FileType) -> Uuid {
    let uuid = Uuid::new_v4();
    repo.insert_file(NewFile {
        uuid,
        path: path.to_string(),
        name: path.rsplit('/').next().unwrap_or(path).to_string(),
        file_type,
        content_hash: Some("0".repeat(64)),
        size_bytes: None,
        mtime: None,
        indexed_at: chrono::Utc::now(),
    })
    .await
    .expect("insert file");
    uuid
}

/// UC-12 / FR-CO-04 — deleting a collection unlinks its items rather than
/// deleting them. That has to hold for a `kind = bookmark` collection too:
/// a bookmark left carrying the `collection_id` of a deleted collection is a
/// dangling reference, and the bookmark is no longer "no longer grouped".
#[tokio::test]
async fn given_bookmark_collection_when_deleted_then_its_bookmarks_are_unlinked() {
    let pool = migrated_pool().await;
    let collections = SqliteCollectionRepository::new(pool.clone());
    let bookmarks = SqliteBookmarkRepository::new(pool.clone());

    let collection_uuid = Uuid::new_v4();
    collections
        .insert_collection(NewCollection {
            uuid: collection_uuid,
            name: "reading".to_string(),
            kind: CollectionKind::Bookmark,
        })
        .await
        .expect("insert collection");

    let bookmark_uuid = Uuid::new_v4();
    bookmarks
        .insert_bookmark(NewBookmark {
            uuid: bookmark_uuid,
            url: "https://example.com/".to_string(),
            title: "Example".to_string(),
            collection_uuid: Some(collection_uuid),
        })
        .await
        .expect("insert bookmark");

    collections
        .delete_collection(collection_uuid)
        .await
        .expect("delete collection");

    // The bookmark survives (UC-12 preserves items) …
    let bookmark = bookmarks
        .find_by_uuid(bookmark_uuid)
        .await
        .expect("find bookmark")
        .expect("bookmark preserved");
    assert_eq!(bookmark.collection_uuid, None, "bookmark is ungrouped");

    // … and carries no stale internal FK either.
    let (dangling,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM bookmarks WHERE collection_id IS NOT NULL")
            .fetch_one(&pool)
            .await
            .expect("query");
    assert_eq!(
        dangling, 0,
        "no bookmark still points at a deleted collection"
    );

    pool.close().await;
}

/// UC-08 / UC-09 — a hard purge removes the file record permanently. The
/// WatchProgress rows that tracked it go with it: they reference the purged
/// `files.id`, and with no FK cascade they would otherwise linger forever,
/// invisible to UC-21 (which inner-joins `files`).
#[tokio::test]
async fn given_video_on_a_watchlist_when_purged_then_its_watch_progress_is_removed() {
    let pool = migrated_pool().await;
    let catalog = SqliteCatalogRepository::new(pool.clone());
    let watchlists = SqliteWatchlistRepository::new(pool.clone());

    let video_uuid = insert_file(&catalog, "/library/movie.mp4", FileType::Video).await;

    let watchlist_uuid = Uuid::new_v4();
    watchlists
        .insert_watchlist(NewWatchlist {
            uuid: watchlist_uuid,
            name: "tonight".to_string(),
        })
        .await
        .expect("insert watchlist");
    watchlists
        .add_video(watchlist_uuid, video_uuid)
        .await
        .expect("add video");

    catalog.purge(video_uuid).await.expect("purge");

    let (remaining,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM watch_progress")
        .fetch_one(&pool)
        .await
        .expect("query");
    assert_eq!(
        remaining, 0,
        "no orphaned watch_progress row survives a purge"
    );

    // The watchlist itself is untouched — only the tracking row went away.
    assert!(watchlists
        .find_by_uuid(watchlist_uuid)
        .await
        .expect("find watchlist")
        .is_some());

    pool.close().await;
}

/// UC-08 / UC-09 — the same guarantee for ReadingProgress (UC-27 inner-joins
/// `files` the same way UC-21 does).
#[tokio::test]
async fn given_item_on_a_reading_list_when_purged_then_its_reading_progress_is_removed() {
    let pool = migrated_pool().await;
    let catalog = SqliteCatalogRepository::new(pool.clone());
    let reading_lists = SqliteReadingListRepository::new(pool.clone());

    let item_uuid = insert_file(&catalog, "/library/book.pdf", FileType::Document).await;

    let reading_list_uuid = Uuid::new_v4();
    reading_lists
        .insert_reading_list(NewReadingList {
            uuid: reading_list_uuid,
            name: "to read".to_string(),
        })
        .await
        .expect("insert reading list");
    reading_lists
        .add_item(reading_list_uuid, item_uuid, ReadingTargetKind::Document)
        .await
        .expect("add item");

    catalog.purge(item_uuid).await.expect("purge");

    let (remaining,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM reading_progress")
        .fetch_one(&pool)
        .await
        .expect("query");
    assert_eq!(
        remaining, 0,
        "no orphaned reading_progress row survives a purge"
    );

    assert!(reading_lists
        .find_by_uuid(reading_list_uuid)
        .await
        .expect("find reading list")
        .is_some());

    pool.close().await;
}
