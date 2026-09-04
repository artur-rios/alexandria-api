//! A library made of a real folder on disk, indexed and then browsed.
//!
//! The gap the other library tests leave: `tree.rs` proves the path
//! arithmetic against paths a test wrote by hand, and `persistence.rs`
//! proves the exclusion against rows a test inserted. Neither walks a
//! folder, so neither can tell whether the files an index records actually
//! reach the library that was made of that folder — which is the whole of
//! what the owner sees.

use alexandria_core::catalog::clock::SystemClock;
use alexandria_core::catalog::commands::index::IndexHandler;
use alexandria_core::catalog::fs::StdFilesystem;
use alexandria_core::catalog::index_scope::IndexScope;
use alexandria_core::catalog::repos::SqliteCatalogRepository;
use alexandria_core::catalog::run_registry::RunRegistry;
use alexandria_core::catalog::runs::SqliteCatalogRunRepository;
use alexandria_core::libraries::commands::RegisterLibraryHandler;
use alexandria_core::libraries::model::LibraryListing;
use alexandria_core::libraries::queries::BrowseLibraryHandler;
use alexandria_core::libraries::repos::SqliteLibraryRepository;
use alexandria_core::migrate::migrate_database;
use sqlx::sqlite::SqlitePool;
use uuid::Uuid;

use crate::common::{
    FakeAudioMetadataReader, FakeAuth, FakeComicMetadataReader, FakeDocumentMetadataReader,
    FakeImageMetadataReader, FakeVideoMetadataReader,
};

/// A course on disk: files at the top, files a level down, and one deeper
/// still — the shape the owner described, and the shape a walk that only
/// read the top level would answer wrongly.
fn course(dir: &std::path::Path) -> std::path::PathBuf {
    let root = dir.join("course");
    std::fs::create_dir_all(root.join("class-01/slides")).expect("mkdir");
    std::fs::create_dir_all(root.join("class-02")).expect("mkdir");

    for (path, bytes) in [
        (root.join("intro.mp3"), &b"audio"[..]),
        (root.join("syllabus.pdf"), &b"%PDF-1.4"[..]),
        (root.join("class-01/lecture.mp4"), &b"video"[..]),
        (root.join("class-01/handout.pdf"), &b"%PDF-1.4"[..]),
        (root.join("class-01/slides/deck.pdf"), &b"%PDF-1.4"[..]),
        (root.join("class-02/lecture.mp4"), &b"video"[..]),
        // Unsupported, and the one thing that should be absent below.
        (root.join("class-02/notes.zip"), &b"zip"[..]),
    ] {
        std::fs::write(path, bytes).expect("write");
    }

    root
}

async fn migrated(dir: &std::path::Path) -> SqlitePool {
    migrate_database(dir.join("alexandria.sqlite").to_str().expect("path"))
        .await
        .expect("migrate")
}

/// Index `root` the way a registration does, with the real walk.
async fn index(pool: &SqlitePool, root: &str) {
    let handler = IndexHandler::new(
        FakeAuth::Allowing,
        SqliteCatalogRepository::new(pool.clone()),
        StdFilesystem,
        SystemClock,
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        4,
        1,
        String::new(),
        SqliteCatalogRunRepository::new(pool.clone()),
        RunRegistry::new(),
    );

    let outcome = handler
        .execute(root, Uuid::new_v4(), &IndexScope::all())
        .await
        .expect("index run");
    assert_eq!(outcome.indexed, 6, "six supported files, one zip skipped");
}

async fn browse(pool: &SqlitePool, uuid: Uuid, path: &str) -> LibraryListing {
    BrowseLibraryHandler::new(
        FakeAuth::Allowing,
        SqliteLibraryRepository::new(pool.clone()),
        SqliteCatalogRepository::new(pool.clone()),
    )
    .browse(uuid, path, "token")
    .await
    .expect("browse")
}

async fn register(pool: &SqlitePool, root: &str) -> Uuid {
    RegisterLibraryHandler::new(
        FakeAuth::Allowing,
        SqliteLibraryRepository::new(pool.clone()),
    )
    .register("Course", root, "token")
    .await
    .expect("register")
    .uuid
}

#[tokio::test]
async fn given_a_library_made_before_the_walk_when_browsed_then_it_holds_what_was_indexed() {
    // The order the application uses: the library exists first, so each file
    // joins it as it is recorded rather than waiting to be claimed.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = course(dir.path());
    let root = root.to_str().expect("root");
    let pool = migrated(dir.path()).await;

    let uuid = register(&pool, root).await;
    index(&pool, root).await;

    let top = browse(&pool, uuid, "").await;
    assert_eq!(
        top.folders
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>(),
        vec!["class-01", "class-02"],
        "the subfolders the files are in"
    );
    assert_eq!(
        names(&top),
        vec!["intro.mp3", "syllabus.pdf"],
        "the files at the top, and only those"
    );

    let class = browse(&pool, uuid, "class-01").await;
    assert_eq!(
        class
            .folders
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>(),
        vec!["slides"]
    );
    assert_eq!(names(&class), vec!["handout.pdf", "lecture.mp4"]);

    let deeper = browse(&pool, uuid, "class-01/slides").await;
    assert_eq!(names(&deeper), vec!["deck.pdf"]);

    let second = browse(&pool, uuid, "class-02").await;
    assert_eq!(
        names(&second),
        vec!["lecture.mp4"],
        "the unsupported zip is not in the catalog, so not in the library"
    );
}

#[tokio::test]
async fn given_a_library_made_after_the_walk_when_browsed_then_it_claims_what_is_there() {
    // The other order, which is what marking an already-indexed folder does:
    // the files are in the catalog before the library exists, and
    // registration claims them.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = course(dir.path());
    let root = root.to_str().expect("root");
    let pool = migrated(dir.path()).await;

    index(&pool, root).await;
    let uuid = register(&pool, root).await;

    let top = browse(&pool, uuid, "").await;
    assert_eq!(names(&top), vec!["intro.mp3", "syllabus.pdf"]);
    assert_eq!(browse(&pool, uuid, "class-01").await.files.len(), 2);
}

#[tokio::test]
async fn given_a_folder_that_was_never_indexed_when_browsed_then_the_library_is_empty() {
    // The failure the owner hit, pinned as the behaviour it is rather than
    // the bug it looks like: a library is browsed out of the catalog, so a
    // folder nobody walked has nothing to show — not even its folders.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = course(dir.path());
    let root = root.to_str().expect("root");
    let pool = migrated(dir.path()).await;

    let uuid = register(&pool, root).await;

    let top = browse(&pool, uuid, "").await;
    assert!(top.files.is_empty());
    assert!(
        top.folders.is_empty(),
        "the folders are derived from indexed paths, not from the disk"
    );
}

/// The file names at one level, sorted so the assertion does not depend on
/// the order the walk happened to return.
fn names(listing: &LibraryListing) -> Vec<String> {
    let mut names: Vec<String> = listing
        .files
        .iter()
        .map(|view| view.file.name.clone())
        .collect();
    names.sort();
    names
}
