//! `SqliteCatalogRepository::ensure_content_hash` against a real migrated
//! SQLite database and a real on-disk file (Task 3 / FR-FC-09). The unit
//! tests in `tests/catalog/` exercise the trait fakes; this file is the
//! precedent-following home (see `tests/repos_integrity.rs`,
//! `tests/thumbnail_cache.rs`) for the two things a fake cannot stand in
//! for: the `UPDATE` actually persisting through a real connection, and a
//! real filesystem read failing.

use alexandria_core::catalog::model::{FileType, NewFile};
use alexandria_core::catalog::repos::{CatalogRepository, SqliteCatalogRepository};
use alexandria_core::errors::DomainError;
use alexandria_core::migrate::run_migrations;
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

async fn insert_file(
    repo: &SqliteCatalogRepository,
    path: &str,
    content_hash: Option<String>,
) -> Uuid {
    let uuid = Uuid::new_v4();
    repo.insert_file(NewFile {
        uuid,
        path: path.to_string(),
        name: path.rsplit('/').next().unwrap_or(path).to_string(),
        file_type: FileType::Text,
        content_hash,
        size_bytes: None,
        mtime: None,
        indexed_at: chrono::Utc::now(),
    })
    .await
    .expect("insert file");
    uuid
}

/// The common case after Task 3: an indexed file's `content_hash` is `None`.
/// The first caller that needs it gets it computed from the real bytes on
/// disk and persisted, so a second read sees it without hashing again.
#[tokio::test]
async fn given_a_file_with_no_stored_hash_when_ensured_then_it_is_computed_and_persisted() {
    let pool = migrated_pool().await;
    let repo = SqliteCatalogRepository::new(pool.clone());

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("notes.txt");
    std::fs::write(&path, b"hello world").expect("write");
    let path_str = path.to_str().expect("utf-8 path").to_string();

    let uuid = insert_file(&repo, &path_str, None).await;

    let hash = repo.ensure_content_hash(uuid).await.expect("ensure");
    assert_eq!(
        hash,
        alexandria_core::catalog::fs::sha256_hex(b"hello world")
    );

    // Persisted, not just returned: a fresh read through the domain layer
    // sees the same value without any further filesystem access.
    let file = repo
        .find_by_uuid(uuid)
        .await
        .expect("find")
        .expect("file present");
    assert_eq!(file.content_hash, Some(hash));

    pool.close().await;
}

/// A file that already carries a hash (UC-33 has edited it, say) must be
/// returned as-is — and, since the path here does not even exist on disk,
/// this also pins that the filesystem is never touched in that case.
#[tokio::test]
async fn given_a_file_with_a_stored_hash_when_ensured_then_the_filesystem_is_not_consulted() {
    let pool = migrated_pool().await;
    let repo = SqliteCatalogRepository::new(pool.clone());

    let uuid = insert_file(
        &repo,
        "/does/not/exist/on/disk.txt",
        Some("preexisting-hash".to_string()),
    )
    .await;

    let hash = repo.ensure_content_hash(uuid).await.expect("ensure");
    assert_eq!(hash, "preexisting-hash");

    pool.close().await;
}

/// A UUID that carries no `files` row at all (`find_by_uuid` miss) is
/// reported as `NotFound`, not silently hashed or treated as "no file".
#[tokio::test]
async fn given_an_unknown_uuid_when_ensured_then_not_found() {
    let pool = migrated_pool().await;
    let repo = SqliteCatalogRepository::new(pool.clone());

    let result = repo.ensure_content_hash(Uuid::new_v4()).await;

    assert!(matches!(result, Err(DomainError::NotFound)));

    pool.close().await;
}

/// A file with no stored hash whose bytes cannot be read (the on-disk file
/// is gone) must surface a disk error, not panic or silently persist a
/// placeholder — the same class of failure `IndexHandler::execute` used to
/// hit hashing at index time, now met here instead.
#[tokio::test]
async fn given_a_file_with_no_stored_hash_and_unreadable_bytes_when_ensured_then_disk_error() {
    let pool = migrated_pool().await;
    let repo = SqliteCatalogRepository::new(pool.clone());

    let dir = tempfile::tempdir().expect("tempdir");
    // Never written: the path is cataloged but nothing is there to read.
    let path = dir.path().join("gone.txt");
    let path_str = path.to_str().expect("utf-8 path").to_string();

    let uuid = insert_file(&repo, &path_str, None).await;

    let result = repo.ensure_content_hash(uuid).await;

    assert!(matches!(result, Err(DomainError::Disk(_))));

    // The failed attempt must not have persisted a stray value.
    let file = repo
        .find_by_uuid(uuid)
        .await
        .expect("find")
        .expect("file present");
    assert_eq!(file.content_hash, None);

    pool.close().await;
}
