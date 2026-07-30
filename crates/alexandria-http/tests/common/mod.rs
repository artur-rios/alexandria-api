//! Shared helpers for alexandria-http integration tests: a temp on-disk
//! SQLite database wired through `build_services`, and a poll helper that
//! waits for the spawned indexing task to persist rows.

use std::time::Duration;

use alexandria_core::config::Settings;
use alexandria_core::migrate::migrate_database;
use alexandria_core::services::{self, Services};
use sqlx::sqlite::SqlitePool;
use tempfile::TempDir;

pub struct TestApp {
    pub services: std::sync::Arc<Services>,
    pub pool: SqlitePool,
    /// Kept alive so the underlying SQLite file isn't deleted mid-run.
    _db_dir: TempDir,
}

pub async fn test_app() -> TestApp {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("alexandria.sqlite");
    let pool = migrate_database(db_path.to_str().expect("path"))
        .await
        .expect("migrate");
    let services = std::sync::Arc::new(services::build_services(&Settings::default(), pool.clone()).await);
    TestApp {
        services,
        pool,
        _db_dir: dir,
    }
}

/// Poll the `files` table until it contains `expected` rows, or time out.
pub async fn wait_for_files(pool: &SqlitePool, expected: i64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files")
            .fetch_one(pool)
            .await
            .expect("count");
        if count.0 >= expected {
            return;
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "timed out waiting for {expected} files; had {} when last checked",
                count.0
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

pub async fn file_rows(pool: &SqlitePool) -> Vec<(String, String, String, String)> {
    sqlx::query_as("SELECT path, name, type, content_hash FROM files ORDER BY path")
        .fetch_all(pool)
        .await
        .expect("rows")
}

/// `(path, name, type, content_hash, missing_at)` — `missing_at` is NULL when
/// the on-disk file was present at last refresh.
pub async fn file_rows_with_missing(
    pool: &SqlitePool,
) -> Vec<(String, String, String, String, Option<String>)> {
    sqlx::query_as(
        "SELECT path, name, type, content_hash, missing_at \
         FROM files ORDER BY path",
    )
    .fetch_all(pool)
    .await
    .expect("rows")
}

pub fn write_file(dir: &tempfile::TempDir, name: &str, contents: &[u8]) -> std::path::PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, contents).expect("write");
    path
}