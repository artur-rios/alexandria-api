//! The file listing across libraries, and within one (FR-FC-12): a search or
//! a trash review can be narrowed to one library's files.

use alexandria_core::catalog::model::{FileType, NewFile};
use alexandria_core::catalog::queries::browse::{BrowseFilesHandler, FileFilter};
use alexandria_core::catalog::repos::{CatalogRepository, SqliteCatalogRepository};
use alexandria_core::migrate::migrate_database;
use chrono::Utc;
use sqlx::sqlite::SqlitePool;
use uuid::Uuid;

use crate::common::{seed_library, FakeAuth};

async fn migrated(dir: &tempfile::TempDir) -> SqlitePool {
    migrate_database(dir.path().join("a.sqlite").to_str().unwrap())
        .await
        .expect("migrate")
}

async fn file(pool: &SqlitePool, library: Uuid, path: &str) {
    SqliteCatalogRepository::new(pool.clone())
        .insert_file(NewFile {
            uuid: Uuid::new_v4(),
            library_uuid: library,
            path: path.to_string(),
            name: path.rsplit('/').next().unwrap().to_string(),
            file_type: FileType::Text,
            content_hash: None,
            size_bytes: None,
            mtime: None,
            indexed_at: Utc::now(),
        })
        .await
        .expect("insert");
}

async fn listed(pool: &SqlitePool, filter: FileFilter) -> Vec<String> {
    let mut paths: Vec<String> = BrowseFilesHandler::new(
        FakeAuth::Allowing,
        SqliteCatalogRepository::new(pool.clone()),
    )
    .list(filter, "token")
    .await
    .expect("list")
    .into_iter()
    .map(|view| view.file.path)
    .collect();
    paths.sort();
    paths
}

async fn two_libraries(pool: &SqlitePool) -> (Uuid, Uuid) {
    let a = seed_library(pool, "/a").await;
    let b = seed_library(pool, "/b").await;
    file(pool, a, "/a/one.md").await;
    file(pool, a, "/a/two.md").await;
    file(pool, b, "/b/three.md").await;
    (a, b)
}

#[tokio::test]
async fn given_two_libraries_when_listed_with_one_library_then_only_its_files() {
    let dir = tempfile::tempdir().unwrap();
    let pool = migrated(&dir).await;
    let (a, b) = two_libraries(&pool).await;

    assert_eq!(
        listed(&pool, FileFilter::new().with_library(a)).await,
        vec!["/a/one.md", "/a/two.md"]
    );
    assert_eq!(
        listed(&pool, FileFilter::new().with_library(b)).await,
        vec!["/b/three.md"]
    );
}

#[tokio::test]
async fn given_library_files_when_listed_without_a_filter_then_all_are_answered() {
    let dir = tempfile::tempdir().unwrap();
    let pool = migrated(&dir).await;
    two_libraries(&pool).await;

    assert_eq!(
        listed(&pool, FileFilter::new()).await,
        vec!["/a/one.md", "/a/two.md", "/b/three.md"]
    );
}

#[tokio::test]
async fn given_an_unknown_library_uuid_when_listed_then_empty() {
    let dir = tempfile::tempdir().unwrap();
    let pool = migrated(&dir).await;
    two_libraries(&pool).await;

    assert!(
        listed(&pool, FileFilter::new().with_library(Uuid::new_v4()))
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn given_a_library_and_a_type_and_a_collection_when_listed_then_every_filter_applies() {
    // The placeholders are appended in one order and bound in another place:
    // all three at once is what would catch them drifting apart.
    let dir = tempfile::tempdir().unwrap();
    let pool = migrated(&dir).await;
    let (a, _) = two_libraries(&pool).await;
    sqlx::raw_sql(
        "INSERT INTO collections (id, uuid, name, kind)
              VALUES (1, '11111111-1111-1111-1111-111111111111', 'C', 'file');
         UPDATE files SET collection_id = 1 WHERE path IN ('/a/one.md', '/b/three.md');",
    )
    .execute(&pool)
    .await
    .unwrap();

    let filter = FileFilter::new()
        .with_type(FileType::Text)
        .with_collection(Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap())
        .with_library(a);

    assert_eq!(listed(&pool, filter).await, vec!["/a/one.md"]);
}
