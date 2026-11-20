//! A library made of a real folder on disk, indexed and then browsed.
//!
//! The gap the other library tests leave: `tree.rs` proves the path
//! arithmetic against paths a test wrote by hand, and `persistence.rs`
//! proves the exclusion against rows a test inserted. Neither walks a
//! folder, so neither can tell whether the files an index records actually
//! reach the library that was made of that folder — which is the whole of
//! what the owner sees.

use alexandria_core::catalog::clock::SystemClock;
use std::sync::Arc;

use alexandria_core::catalog::commands::index::{IndexHandler, IndexOutcome};
use alexandria_core::catalog::fs::StdFilesystem;
use alexandria_core::catalog::index_scope::IndexScope;
use alexandria_core::catalog::library_root::LibraryRoot;
use alexandria_core::catalog::repos::SqliteCatalogRepository;
use alexandria_core::catalog::run_registry::RunRegistry;
use alexandria_core::catalog::runs::SqliteCatalogRunRepository;
use alexandria_core::catalog::runs::{CatalogRunRepository, RunKind, RunPriority, RunStatus};
use alexandria_core::errors::DomainError;
use alexandria_core::libraries::commands::{
    IndexLibraryHandler, MoveLibraryHandler, RegisterLibraryHandler,
};
use alexandria_core::libraries::model::{LibraryListing, RegisteredLibrary};
use alexandria_core::libraries::queries::BrowseLibraryHandler;
use alexandria_core::libraries::repos::LibraryRepository;
use alexandria_core::libraries::repos::SqliteLibraryRepository;
use alexandria_core::migrate::migrate_database;
use sqlx::sqlite::SqlitePool;
use uuid::Uuid;

use crate::common::{
    FakeAuth, FakeComicMetadataReader, FakeDocumentMetadataReader, FakeImageMetadataReader,
    FakeVideoMetadataReader,
};

/// A course on disk: files at the top, files a level down, and one deeper
/// still — the shape the owner described, and the shape a walk that only
/// read the top level would answer wrongly.
fn course(dir: &std::path::Path) -> std::path::PathBuf {
    let root = dir.join("course");
    std::fs::create_dir_all(root.join("class-01/slides")).expect("mkdir");
    std::fs::create_dir_all(root.join("class-02")).expect("mkdir");

    for (path, bytes) in [
        (root.join("intro.txt"), &b"notes"[..]),
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

/// The index handler a registration starts its run through, bounded by
/// `filesystem_root` (empty is unbounded).
type TestIndex = IndexHandler<
    FakeAuth,
    SqliteCatalogRepository,
    StdFilesystem,
    SystemClock,
    FakeImageMetadataReader,
    FakeDocumentMetadataReader,
    FakeVideoMetadataReader,
    FakeComicMetadataReader,
    SqliteCatalogRunRepository,
>;

fn index_handler(pool: &SqlitePool, filesystem_root: &str) -> Arc<TestIndex> {
    Arc::new(IndexHandler::new(
        FakeAuth::Allowing,
        SqliteCatalogRepository::new(pool.clone()),
        StdFilesystem,
        SystemClock,
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        4,
        1,
        filesystem_root.to_string(),
        SqliteCatalogRunRepository::new(pool.clone()),
        RunRegistry::new(),
    ))
}

fn registrar(
    pool: &SqlitePool,
    filesystem_root: &str,
) -> RegisterLibraryHandler<FakeAuth, SqliteLibraryRepository, Arc<TestIndex>> {
    RegisterLibraryHandler::new(
        FakeAuth::Allowing,
        SqliteLibraryRepository::new(pool.clone()),
        index_handler(pool, filesystem_root),
        LibraryRoot::new(filesystem_root),
    )
}

fn indexer(
    pool: &SqlitePool,
) -> IndexLibraryHandler<
    FakeAuth,
    SqliteLibraryRepository,
    Arc<TestIndex>,
    SqliteCatalogRunRepository,
> {
    IndexLibraryHandler::new(
        FakeAuth::Allowing,
        SqliteLibraryRepository::new(pool.clone()),
        index_handler(pool, ""),
        SqliteCatalogRunRepository::new(pool.clone()),
    )
}

fn mover(
    pool: &SqlitePool,
) -> MoveLibraryHandler<FakeAuth, SqliteLibraryRepository, SqliteCatalogRunRepository> {
    MoveLibraryHandler::new(
        FakeAuth::Allowing,
        SqliteLibraryRepository::new(pool.clone()),
        LibraryRoot::new(""),
        SqliteCatalogRunRepository::new(pool.clone()),
    )
}

async fn register(pool: &SqlitePool, root: &str) -> Uuid {
    registrar(pool, "")
        .register("Course", root, RunPriority::Normal, "token")
        .await
        .expect("register")
        .library
        .uuid
}

/// Register `root` and walk the run the registration opened, as a transport
/// does by spawning it.
async fn register_and_walk(pool: &SqlitePool, root: &str) -> (RegisteredLibrary, IndexOutcome) {
    let registered = registrar(pool, "")
        .register("Course", root, RunPriority::Normal, "token")
        .await
        .expect("register");
    let outcome = index_handler(pool, "")
        .execute(
            &registered.library.root_path,
            registered.run_id,
            &IndexScope::all(),
        )
        .await
        .expect("walk");
    (registered, outcome)
}

async fn pause(pool: &SqlitePool, run_id: Uuid) {
    sqlx::query("UPDATE catalog_runs SET status = 'paused' WHERE id = ?")
        .bind(run_id.to_string())
        .execute(pool)
        .await
        .expect("pause");
}

async fn file_libraries(pool: &SqlitePool) -> Vec<Option<String>> {
    sqlx::query_scalar("SELECT (SELECT uuid FROM libraries WHERE id = files.library_id) FROM files")
        .fetch_all(pool)
        .await
        .expect("files")
}

#[tokio::test]
async fn given_a_library_made_before_the_walk_when_browsed_then_it_holds_what_was_indexed() {
    // The order the application uses: the library exists first, so each file
    // joins it as it is recorded rather than waiting to be claimed.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = course(dir.path());
    let root = root.to_str().expect("root");
    let pool = migrated(dir.path()).await;

    let (registered, _) = register_and_walk(&pool, root).await;
    let uuid = registered.library.uuid;

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
        vec!["intro.txt", "syllabus.pdf"],
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
    assert_eq!(names(&top), vec!["intro.txt", "syllabus.pdf"]);
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

/// A folder with two readable files and one Alexandria cannot read.
fn mixed(dir: &std::path::Path) -> std::path::PathBuf {
    let root = dir.join("mixed");
    std::fs::create_dir_all(&root).expect("mkdir");
    for (name, bytes) in [
        ("a.md", &b"# a"[..]),
        ("b.pdf", &b"%PDF-1.4"[..]),
        ("c.zip", &b"zip"[..]),
    ] {
        std::fs::write(root.join(name), bytes).expect("write");
    }
    root
}

#[tokio::test]
async fn given_a_folder_when_registered_then_a_run_indexes_its_readable_files_and_skips_the_rest() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = mixed(dir.path());
    let pool = migrated(dir.path()).await;

    let (registered, outcome) = register_and_walk(&pool, root.to_str().unwrap()).await;

    assert_eq!(outcome.indexed, 2);
    assert_eq!(outcome.skipped, 1);
    let run = SqliteCatalogRunRepository::new(pool.clone())
        .get(registered.run_id)
        .await
        .expect("get")
        .expect("the registration opened a run");
    assert_eq!(run.status, RunStatus::Complete);
    assert_eq!(run.library_uuid, Some(registered.library.uuid));
    assert_eq!(
        file_libraries(&pool).await,
        vec![Some(registered.library.uuid.to_string()); 2]
    );
}

#[tokio::test]
async fn given_a_root_with_a_trailing_separator_when_registered_then_its_run_indexes_the_folder() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = mixed(dir.path());
    let root = root.to_str().unwrap().to_string();
    let pool = migrated(dir.path()).await;

    let (registered, outcome) = register_and_walk(&pool, &format!("{root}/")).await;

    assert_eq!(registered.library.root_path, root);
    assert_eq!(outcome.indexed, 2);
    assert_eq!(
        file_libraries(&pool).await,
        vec![Some(registered.library.uuid.to_string()); 2]
    );
}

#[tokio::test]
async fn given_a_folder_that_does_not_exist_when_registered_then_invalid_input_and_nothing_is_stored(
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = migrated(dir.path()).await;
    let missing = dir.path().join("nowhere");

    let result = registrar(&pool, "")
        .register(
            "Gone",
            missing.to_str().unwrap(),
            RunPriority::Normal,
            "token",
        )
        .await;

    assert!(
        matches!(result, Err(DomainError::InvalidInput(_))),
        "{result:?}"
    );
    assert_nothing_stored(&pool).await;
}

#[tokio::test]
async fn given_a_folder_outside_filesystem_root_when_registered_then_refused_and_nothing_is_stored()
{
    let dir = tempfile::tempdir().expect("tempdir");
    let bound = dir.path().join("bound");
    std::fs::create_dir_all(&bound).expect("mkdir");
    let outside = mixed(dir.path());
    let pool = migrated(dir.path()).await;

    let result = registrar(&pool, bound.to_str().unwrap())
        .register(
            "Out",
            outside.to_str().unwrap(),
            RunPriority::Normal,
            "token",
        )
        .await;

    assert!(result.is_err(), "{result:?}");
    assert_nothing_stored(&pool).await;
}

#[tokio::test]
async fn given_an_overlapping_folder_when_registered_then_conflict_naming_the_library_and_no_run() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = course(dir.path());
    let pool = migrated(dir.path()).await;
    register(&pool, root.to_str().unwrap()).await;

    let result = registrar(&pool, "")
        .register(
            "Inner",
            root.join("class-01").to_str().unwrap(),
            RunPriority::Normal,
            "token",
        )
        .await;

    match result {
        Err(DomainError::Conflict(message)) => assert!(message.contains("Course"), "{message}"),
        other => panic!("expected a conflict, got {other:?}"),
    }
    let runs = SqliteCatalogRunRepository::new(pool.clone())
        .list_active()
        .await
        .expect("active");
    assert_eq!(runs.len(), 1, "only the first library's run");
}

#[tokio::test]
async fn given_an_indexed_library_when_reindexed_then_existing_files_count_as_already_cataloged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = mixed(dir.path());
    let pool = migrated(dir.path()).await;
    let (registered, _) = register_and_walk(&pool, root.to_str().unwrap()).await;

    let (library, started) = indexer(&pool)
        .index(registered.library.uuid, RunPriority::Normal, "token")
        .await
        .expect("reindex");
    let outcome = index_handler(&pool, "")
        .execute(&library.root_path, started.run_id, &IndexScope::all())
        .await
        .expect("walk");

    assert_eq!(outcome.already_cataloged, 2);
    assert_eq!(outcome.failed, 0);
    assert_eq!(file_libraries(&pool).await.len(), 2);
}

#[tokio::test]
async fn given_a_paused_run_when_the_library_is_reindexed_then_conflict_naming_the_run() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = mixed(dir.path());
    let pool = migrated(dir.path()).await;
    let registered = registrar(&pool, "")
        .register(
            "Mixed",
            root.to_str().unwrap(),
            RunPriority::Normal,
            "token",
        )
        .await
        .expect("register");
    pause(&pool, registered.run_id).await;

    let result = indexer(&pool)
        .index(registered.library.uuid, RunPriority::Normal, "token")
        .await;

    match result {
        Err(DomainError::Conflict(message)) => {
            assert!(
                message.contains(&registered.run_id.to_string()),
                "{message}"
            )
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
}

#[tokio::test]
async fn given_an_unknown_library_when_reindexed_then_not_found() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = migrated(dir.path()).await;

    let result = indexer(&pool)
        .index(Uuid::new_v4(), RunPriority::Normal, "token")
        .await;

    assert!(matches!(result, Err(DomainError::NotFound)), "{result:?}");
}

#[tokio::test]
async fn given_a_paused_run_when_the_library_is_moved_then_conflict_naming_the_run() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = mixed(dir.path());
    let pool = migrated(dir.path()).await;
    let registered = registrar(&pool, "")
        .register(
            "Mixed",
            root.to_str().unwrap(),
            RunPriority::Normal,
            "token",
        )
        .await
        .expect("register");
    pause(&pool, registered.run_id).await;

    let result = mover(&pool)
        .move_to(
            registered.library.uuid,
            dir.path().join("elsewhere").to_str().unwrap(),
            "token",
        )
        .await;

    match result {
        Err(DomainError::Conflict(message)) => {
            assert!(
                message.contains(&registered.run_id.to_string()),
                "{message}"
            )
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
}

#[tokio::test]
async fn given_a_library_run_when_its_status_is_read_then_it_carries_the_library_uuid() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = mixed(dir.path());
    let pool = migrated(dir.path()).await;
    let registered = registrar(&pool, "")
        .register(
            "Mixed",
            root.to_str().unwrap(),
            RunPriority::Normal,
            "token",
        )
        .await
        .expect("register");
    let runs = SqliteCatalogRunRepository::new(pool.clone());

    let run = runs
        .get(registered.run_id)
        .await
        .expect("get")
        .expect("run");
    let active = runs.list_active().await.expect("active");

    assert_eq!(run.library_uuid, Some(registered.library.uuid));
    assert_eq!(active[0].library_uuid, Some(registered.library.uuid));
    let json = serde_json::to_value(&run).expect("json");
    assert_eq!(json["libraryUuid"], registered.library.uuid.to_string());
}

#[tokio::test]
async fn given_a_refresh_run_when_its_status_is_read_then_it_has_no_library() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = migrated(dir.path()).await;
    let runs = SqliteCatalogRunRepository::new(pool.clone());
    let id = Uuid::new_v4();
    runs.start(
        id,
        RunKind::Refresh,
        None,
        chrono::Utc::now(),
        1,
        None,
        None,
    )
    .await
    .expect("start");

    let run = runs.get(id).await.expect("get").expect("run");

    assert_eq!(run.library_uuid, None);
    let json = serde_json::to_value(&run).expect("json");
    assert!(json.get("libraryUuid").is_none(), "{json}");
}

async fn assert_nothing_stored(pool: &SqlitePool) {
    let libraries = SqliteLibraryRepository::new(pool.clone())
        .list_all()
        .await
        .expect("libraries");
    assert!(libraries.is_empty(), "{libraries:?}");
    let runs = SqliteCatalogRunRepository::new(pool.clone())
        .list_active()
        .await
        .expect("runs");
    assert!(runs.is_empty(), "{runs:?}");
}
