//! Task 4 correction 2 — a text edit's post-write refresh must record the
//! edited file's new size and mtime, not just its hash.
//!
//! Re-index (`RefreshHandler`, Task 4) decides "did this change?" purely by
//! comparing stat (size + mtime), never bytes. `EditTextFileContentHandler`
//! (UC-33) rewrites a text file's bytes on disk, which changes both its size
//! and its mtime. If the post-write `refresh_hash` recorded only the new
//! `content_hash` and left the row's *pre-edit* size/mtime in place, the very
//! next re-index would see a stat mismatch, count the file UC-33 just edited
//! as "changed", and null out the hash that edit had just verified and
//! stored — a false positive that also destroys real data.
//!
//! This is exercised against a real file on disk and a real migrated SQLite
//! database rather than the trait fakes, because the bug only shows up when
//! an actual write actually changes an actual mtime — `repos_integrity.rs` is
//! this crate's precedent for a real SQLite pool in an integration test,
//! `hashing.rs` for touching the real filesystem directly.

use alexandria_core::auth::BearerAuthService;
use alexandria_core::catalog::clock::SystemClock;
use alexandria_core::catalog::commands::edit_content::EditTextFileContentHandler;
use alexandria_core::catalog::commands::refresh::RefreshHandler;
use alexandria_core::catalog::fs::StdFilesystem;
use alexandria_core::catalog::model::{FileType, NewFile};
use alexandria_core::catalog::repos::{CatalogRepository, SqliteCatalogRepository};
use alexandria_core::catalog::runs::SqliteCatalogRunRepository;
use alexandria_core::migrate::run_migrations;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use uuid::Uuid;

const TOKEN: &str = "bearer-token";

async fn migrated_pool() -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");
    run_migrations(&pool).await.expect("migrate");
    pool
}

#[tokio::test]
async fn given_a_just_edited_text_file_when_refreshed_then_it_is_unchanged_and_its_hash_is_kept() {
    // Arrange: a text file cataloged the way indexing (Task 3) leaves one —
    // no stored hash, no stat.
    let pool = migrated_pool().await;
    let repo = SqliteCatalogRepository::new(pool.clone());
    let runs = SqliteCatalogRunRepository::new(pool);

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("notes.txt");
    std::fs::write(&path, "before").expect("seed file on disk");
    let path_str = path.to_str().expect("utf-8 path").to_string();

    let uuid = Uuid::new_v4();
    repo.insert_file(NewFile {
        uuid,
        path: path_str,
        name: "notes.txt".to_string(),
        file_type: FileType::Text,
        content_hash: None,
        size_bytes: None,
        mtime: None,
        indexed_at: chrono::Utc::now(),
    })
    .await
    .expect("insert file");

    // Act 1: UC-33 edits the file's content — this is a real write, so the
    // file's on-disk size and mtime genuinely change.
    let editor = EditTextFileContentHandler::new(
        BearerAuthService,
        repo.clone(),
        StdFilesystem,
        SystemClock,
    );
    let edited = editor
        .edit(
            uuid,
            "after — deliberately longer than before".to_string(),
            TOKEN,
        )
        .await
        .expect("edit");
    assert!(
        edited.content_hash.is_some(),
        "the edit itself must record a hash"
    );

    // Act 2: re-index runs immediately afterward, as it would on the next
    // scheduled pass — nothing else has touched the file in between.
    let refresher = RefreshHandler::new(
        BearerAuthService,
        repo.clone(),
        StdFilesystem,
        SystemClock,
        1,
        runs,
    );
    let started = refresher.start(TOKEN).await.expect("start");
    let outcome = refresher.execute(started.run_id).await.expect("execute");

    // Assert: the edit's own post-write stat already matches what re-index
    // observes, so nothing looks changed — and, critically, the hash UC-33
    // just verified survives. Without Correction 2 this file shows up
    // `refreshed` and its `content_hash` comes back `None`.
    assert_eq!(
        outcome.unchanged, 1,
        "the edit's post-write stat must already satisfy the very next refresh"
    );
    assert_eq!(outcome.refreshed, 0);

    let after = repo
        .find_by_uuid(uuid)
        .await
        .expect("query")
        .expect("file still cataloged");
    assert!(
        after.content_hash.is_some(),
        "a stat-matching refresh must not clobber the hash the edit just verified"
    );
    assert_eq!(after.content_hash, edited.content_hash);
}
