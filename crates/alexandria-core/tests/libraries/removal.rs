//! Removing a library deletes its catalog records (libraries-only design,
//! section 3): every file row in it and everything attached to those rows,
//! its index runs, and the library itself — never a file on disk, and never
//! another library's records.

use alexandria_core::catalog::model::{FileType, NewFile};
use alexandria_core::catalog::repos::{CatalogRepository, SqliteCatalogRepository};
use alexandria_core::catalog::runs::{CatalogRunRepository, RunKind, SqliteCatalogRunRepository};
use alexandria_core::errors::DomainError;
use alexandria_core::libraries::commands::RemoveLibraryHandler;
use alexandria_core::libraries::repos::SqliteLibraryRepository;
use alexandria_core::migrate::migrate_database;
use chrono::Utc;
use sqlx::sqlite::SqlitePool;
use uuid::Uuid;

use crate::common::seed_library;

async fn migrated(dir: &tempfile::TempDir) -> SqlitePool {
    migrate_database(dir.path().join("a.sqlite").to_str().unwrap())
        .await
        .expect("migrate")
}

/// One file row in `library`, answering its internal id.
async fn file(pool: &SqlitePool, library: Uuid, path: &str, file_type: FileType) -> i64 {
    let uuid = Uuid::new_v4();
    SqliteCatalogRepository::new(pool.clone())
        .insert_file(NewFile {
            uuid,
            library_uuid: library,
            path: path.to_string(),
            name: path.rsplit('/').next().unwrap().to_string(),
            file_type,
            content_hash: None,
            size_bytes: None,
            mtime: None,
            indexed_at: Utc::now(),
        })
        .await
        .expect("insert");
    sqlx::query_scalar("SELECT id FROM files WHERE uuid = ?")
        .bind(uuid.to_string())
        .fetch_one(pool)
        .await
        .expect("id")
}

async fn ids(pool: &SqlitePool, sql: &str) -> Vec<i64> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
        .fetch_all(pool)
        .await
        .expect("ids")
}

fn remover(
    pool: &SqlitePool,
) -> RemoveLibraryHandler<SqliteLibraryRepository, SqliteCatalogRunRepository> {
    RemoveLibraryHandler::new(
        SqliteLibraryRepository::new(pool.clone()),
        SqliteCatalogRunRepository::new(pool.clone()),
    )
}

#[tokio::test]
async fn given_a_library_in_a_collection_and_lists_when_removed_then_its_items_go_and_the_groupings_stay(
) {
    let dir = tempfile::tempdir().unwrap();
    let pool = migrated(&dir).await;
    let a = seed_library(&pool, "/a").await;
    let b = seed_library(&pool, "/b").await;
    let a_video = file(&pool, a, "/a/film.mp4", FileType::Video).await;
    let a_pdf = file(&pool, a, "/a/book.pdf", FileType::Document).await;
    let b_video = file(&pool, b, "/b/film.mp4", FileType::Video).await;
    let b_pdf = file(&pool, b, "/b/book.pdf", FileType::Document).await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "INSERT INTO collections (id, uuid, name, kind) VALUES (1, 'c', 'Both', 'file');
         UPDATE files SET collection_id = 1 WHERE id IN ({a_video}, {b_video});
         INSERT INTO watchlists (id, uuid, name) VALUES (1, 'w', 'Later');
         INSERT INTO watch_progress (watchlist_id, video_file_id, state)
              VALUES (1, {a_video}, 'watching'), (1, {b_video}, 'watching');
         INSERT INTO reading_lists (id, uuid, name) VALUES (1, 'r', 'Books');
         INSERT INTO reading_progress (reading_list_id, item_file_id, target_kind, state)
              VALUES (1, {a_pdf}, 'document', 'reading'), (1, {b_pdf}, 'document', 'reading');"
    )))
    .execute(&pool)
    .await
    .expect("seed");

    remover(&pool).remove(a).await.expect("remove");

    assert_eq!(
        ids(&pool, "SELECT id FROM files ORDER BY id").await,
        vec![b_video, b_pdf],
        "A's files are gone and B's are untouched"
    );
    assert_eq!(ids(&pool, "SELECT id FROM collections").await, vec![1]);
    assert_eq!(
        ids(&pool, "SELECT id FROM files WHERE collection_id = 1").await,
        vec![b_video]
    );
    assert_eq!(ids(&pool, "SELECT id FROM watchlists").await, vec![1]);
    assert_eq!(
        ids(&pool, "SELECT video_file_id FROM watch_progress").await,
        vec![b_video]
    );
    assert_eq!(ids(&pool, "SELECT id FROM reading_lists").await, vec![1]);
    assert_eq!(
        ids(&pool, "SELECT item_file_id FROM reading_progress").await,
        vec![b_pdf]
    );
    assert_eq!(
        ids(&pool, "SELECT file_id FROM video_files ORDER BY file_id").await,
        vec![b_video],
        "the subtype rows went with their files"
    );
    let libraries: Vec<String> = sqlx::query_scalar("SELECT uuid FROM libraries")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(libraries, vec![b.to_string()]);
}

#[tokio::test]
async fn given_a_library_when_removed_then_every_file_stays_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let pool = migrated(&dir).await;
    let root = dir.path().join("course");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("notes.md"), b"# notes").unwrap();
    let library = seed_library(&pool, root.to_str().unwrap()).await;
    let path = root.join("notes.md");
    file(&pool, library, path.to_str().unwrap(), FileType::Text).await;

    remover(&pool).remove(library).await.expect("remove");

    assert_eq!(std::fs::read(&path).unwrap(), b"# notes");
    assert!(ids(&pool, "SELECT id FROM files").await.is_empty());
}

#[tokio::test]
async fn given_a_running_run_when_the_library_is_removed_then_conflict_and_nothing_is_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let pool = migrated(&dir).await;
    let library = seed_library(&pool, "/a").await;
    let kept = file(&pool, library, "/a/film.mp4", FileType::Video).await;
    let run_id = Uuid::new_v4();
    SqliteCatalogRunRepository::new(pool.clone())
        .start(
            run_id,
            RunKind::Index,
            Some("/a"),
            Utc::now(),
            1,
            Some(library),
        )
        .await
        .unwrap();

    let result = remover(&pool).remove(library).await;

    match result {
        Err(DomainError::Conflict(message)) => {
            assert!(message.contains(&run_id.to_string()), "{message}")
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
    assert_eq!(ids(&pool, "SELECT id FROM files").await, vec![kept]);
    assert_eq!(ids(&pool, "SELECT id FROM libraries").await.len(), 1);
}

#[tokio::test]
async fn given_a_paused_run_when_the_library_is_removed_then_the_run_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let pool = migrated(&dir).await;
    let library = seed_library(&pool, "/a").await;
    let runs = SqliteCatalogRunRepository::new(pool.clone());
    let run_id = Uuid::new_v4();
    runs.start(
        run_id,
        RunKind::Index,
        Some("/a"),
        Utc::now(),
        1,
        Some(library),
    )
    .await
    .unwrap();
    runs.record_failure(run_id, "/a/locked.md", "locked", Utc::now())
        .await
        .unwrap();
    sqlx::query("UPDATE catalog_runs SET status = 'paused' WHERE id = ?")
        .bind(run_id.to_string())
        .execute(&pool)
        .await
        .unwrap();

    remover(&pool).remove(library).await.expect("remove");

    assert!(runs.get(run_id).await.unwrap().is_none());
    assert!(ids(&pool, "SELECT id FROM catalog_run_failures")
        .await
        .is_empty());
}

#[tokio::test]
async fn given_a_refresh_run_when_a_library_is_removed_then_the_refresh_run_remains() {
    let dir = tempfile::tempdir().unwrap();
    let pool = migrated(&dir).await;
    let a = seed_library(&pool, "/a").await;
    seed_library(&pool, "/b").await;
    let runs = SqliteCatalogRunRepository::new(pool.clone());
    let refresh = Uuid::new_v4();
    runs.start(refresh, RunKind::Refresh, None, Utc::now(), 1, None)
        .await
        .unwrap();
    runs.record_failure(refresh, "/a/gone.md", "locked", Utc::now())
        .await
        .unwrap();
    runs.record_failure(refresh, "/b/kept.md", "locked", Utc::now())
        .await
        .unwrap();

    remover(&pool).remove(a).await.expect("remove");

    assert!(runs.get(refresh).await.unwrap().is_some());
    let failures: Vec<String> =
        sqlx::query_scalar("SELECT path FROM catalog_run_failures ORDER BY path")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        failures,
        vec!["/b/kept.md".to_string()],
        "the removed library's failures went with its files"
    );
}

#[tokio::test]
async fn given_an_unknown_library_when_removed_then_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let pool = migrated(&dir).await;

    let result = remover(&pool).remove(Uuid::new_v4()).await;

    assert!(matches!(result, Err(DomainError::NotFound)), "{result:?}");
}
