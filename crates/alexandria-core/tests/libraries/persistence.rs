//! Libraries against a real migrated database (Testing Specification §6.4) —
//! proves migration 20 applies, and that the exclusion is real rather than
//! sound in principle.

use alexandria_core::catalog::model::{FileType, NewFile, StateFilter};
use alexandria_core::catalog::repos::{CatalogRepository, SqliteCatalogRepository};
use alexandria_core::errors::DomainError;
use alexandria_core::libraries::commands::{RegisterLibraryHandler, RemoveLibraryHandler};
use alexandria_core::libraries::queries::BrowseLibraryHandler;
use alexandria_core::libraries::repos::SqliteLibraryRepository;
use alexandria_core::migrate::migrate_database;
use chrono::Utc;
use uuid::Uuid;

use crate::common::FakeAuth;

const ROOT: &str = "/library/course";

async fn fixtures() -> (
    sqlx::sqlite::SqlitePool,
    SqliteCatalogRepository,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = migrate_database(dir.path().join("a.sqlite").to_str().unwrap())
        .await
        .expect("migrate");
    (pool.clone(), SqliteCatalogRepository::new(pool), dir)
}

async fn insert(catalog: &SqliteCatalogRepository, path: &str, file_type: FileType) -> Uuid {
    let uuid = Uuid::new_v4();
    catalog
        .insert_file(NewFile {
            uuid,
            path: path.to_string(),
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            file_type,
            content_hash: Some("0".repeat(64)),
            size_bytes: None,
            mtime: None,
            indexed_at: Utc::now(),
        })
        .await
        .expect("insert");
    uuid
}

#[tokio::test]
async fn given_a_registered_library_when_the_videos_are_listed_then_its_files_are_absent() {
    // The whole point: a hundred lecture recordings must not bury the films.
    let (pool, catalog, _dir) = fixtures().await;
    insert(
        &catalog,
        "/library/course/class-01/lecture.mp4",
        FileType::Video,
    )
    .await;
    insert(&catalog, "/library/films/a-film.mkv", FileType::Video).await;

    RegisterLibraryHandler::new(
        FakeAuth::Allowing,
        SqliteLibraryRepository::new(pool.clone()),
    )
    .register("Course", ROOT, "token")
    .await
    .expect("register");

    let listed = catalog
        .list_filtered_view(Some(FileType::Video), StateFilter::Active, None)
        .await
        .expect("list");

    assert_eq!(listed.len(), 1, "a library's file was still in the panel");
    assert_eq!(listed[0].file.name, "a-film.mkv");
}

#[tokio::test]
async fn given_a_library_when_it_is_removed_then_its_files_come_back() {
    // Marking a folder empties part of a panel, and that is not visible
    // until after it is done. The way back has to restore, not delete.
    let (pool, catalog, _dir) = fixtures().await;
    insert(
        &catalog,
        "/library/course/class-01/lecture.mp4",
        FileType::Video,
    )
    .await;

    let library = RegisterLibraryHandler::new(
        FakeAuth::Allowing,
        SqliteLibraryRepository::new(pool.clone()),
    )
    .register("Course", ROOT, "token")
    .await
    .expect("register");

    RemoveLibraryHandler::new(
        FakeAuth::Allowing,
        SqliteLibraryRepository::new(pool.clone()),
    )
    .remove(library.uuid, "token")
    .await
    .expect("remove");

    let listed = catalog
        .list_filtered_view(Some(FileType::Video), StateFilter::Active, None)
        .await
        .expect("list");

    assert_eq!(listed.len(), 1, "removing a library lost its files");
}

#[tokio::test]
async fn given_an_overlapping_folder_when_registered_then_it_is_refused_by_name() {
    // Two libraries owning one file means two answers to "where does this
    // appear". Refused with the existing one named, so the owner can act.
    let (pool, _catalog, _dir) = fixtures().await;
    let handler = RegisterLibraryHandler::new(
        FakeAuth::Allowing,
        SqliteLibraryRepository::new(pool.clone()),
    );
    handler
        .register("Course", ROOT, "token")
        .await
        .expect("register");

    let nested = handler
        .register("Week one", "/library/course/class-01", "token")
        .await;

    match nested {
        Err(DomainError::Conflict(message)) => assert!(message.contains("Course"), "{message}"),
        other => panic!("expected a conflict naming the existing library, got {other:?}"),
    }
}

#[tokio::test]
async fn given_a_sibling_folder_with_a_shared_prefix_when_registered_then_it_is_allowed() {
    // `/library/course` must not be treated as containing
    // `/library/course-notes`: a different folder that merely starts with
    // the same letters.
    let (pool, _catalog, _dir) = fixtures().await;
    let handler = RegisterLibraryHandler::new(
        FakeAuth::Allowing,
        SqliteLibraryRepository::new(pool.clone()),
    );
    handler
        .register("Course", ROOT, "token")
        .await
        .expect("first");

    let sibling = handler
        .register("Notes", "/library/course-notes", "token")
        .await;

    assert!(sibling.is_ok(), "a sibling folder was refused: {sibling:?}");
}

#[tokio::test]
async fn given_files_already_indexed_when_a_library_is_registered_then_they_are_claimed() {
    // A folder is usually marked *after* it has been indexed. A library that
    // showed nothing until the owner re-walked their disk would read as
    // broken.
    let (pool, catalog, _dir) = fixtures().await;
    insert(
        &catalog,
        "/library/course/class-01/lecture.mp4",
        FileType::Video,
    )
    .await;
    insert(&catalog, "/library/course/syllabus.pdf", FileType::Document).await;

    let library = RegisterLibraryHandler::new(
        FakeAuth::Allowing,
        SqliteLibraryRepository::new(pool.clone()),
    )
    .register("Course", ROOT, "token")
    .await
    .expect("register");

    let listing = BrowseLibraryHandler::new(
        FakeAuth::Allowing,
        SqliteLibraryRepository::new(pool.clone()),
        SqliteCatalogRepository::new(pool.clone()),
    )
    .browse(library.uuid, "", "token")
    .await
    .expect("browse");

    assert_eq!(listing.folders.len(), 1, "the class folder was not seen");
    assert_eq!(listing.files.len(), 1, "the syllabus was not at the top");
}
