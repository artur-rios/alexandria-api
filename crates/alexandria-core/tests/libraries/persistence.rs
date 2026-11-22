//! Libraries against a real migrated database (Testing Specification §6.4) —
//! proves migration 20 applies, and that the exclusion is real rather than
//! sound in principle.

use alexandria_core::catalog::model::{FileType, NewFile};
use alexandria_core::catalog::repos::{CatalogRepository, SqliteCatalogRepository};
use alexandria_core::errors::DomainError;
use alexandria_core::libraries::commands::RegisterLibraryHandler;
use alexandria_core::libraries::model::LibraryListing;
use alexandria_core::libraries::queries::BrowseLibraryHandler;
use alexandria_core::libraries::repos::SqliteLibraryRepository;
use alexandria_core::migrate::migrate_database;
use chrono::Utc;
use uuid::Uuid;

use alexandria_core::catalog::library_root::LibraryRoot;
use alexandria_core::catalog::runs::{RunPriority, SqliteCatalogRunRepository};

use crate::common::AlwaysStartsRun;

const ROOT: &str = "/library/course";

/// The catalog repository, with the pool beside it so a test file can be
/// put into the library that holds its path.
struct Catalog {
    repo: SqliteCatalogRepository,
    pool: sqlx::sqlite::SqlitePool,
}

impl std::ops::Deref for Catalog {
    type Target = SqliteCatalogRepository;
    fn deref(&self) -> &SqliteCatalogRepository {
        &self.repo
    }
}

async fn fixtures() -> (sqlx::sqlite::SqlitePool, Catalog, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = migrate_database(dir.path().join("a.sqlite").to_str().unwrap())
        .await
        .expect("migrate");
    let catalog = Catalog {
        repo: SqliteCatalogRepository::new(pool.clone()),
        pool: pool.clone(),
    };
    (pool, catalog, dir)
}

/// Put one file at `path` into the library whose folder holds it — what an
/// index run of that library records. Every file belongs to a library, so a
/// path no library holds is the test's own mistake.
async fn insert(catalog: &Catalog, path: &str, file_type: FileType) -> Uuid {
    let libraries: Vec<(String, String)> = sqlx::query_as("SELECT uuid, root_path FROM libraries")
        .fetch_all(&catalog.pool)
        .await
        .expect("libraries");
    let slashed = |p: &str| p.replace('\\', "/");
    let library = libraries
        .iter()
        .filter(|(_, root)| {
            let root = format!("{}/", slashed(root).trim_end_matches('/'));
            slashed(path).starts_with(&root)
        })
        .max_by_key(|(_, root)| root.len())
        .map(|(uuid, _)| Uuid::parse_str(uuid).expect("uuid"))
        .unwrap_or_else(|| panic!("no library holds {path}; register one first"));

    let uuid = Uuid::new_v4();
    catalog
        .insert_file(NewFile {
            uuid,
            library_uuid: library,
            path: path.to_string(),
            name: path.rsplit(['/', '\\']).next().unwrap_or(path).to_string(),
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
async fn given_an_overlapping_folder_when_registered_then_it_is_refused_by_name() {
    // Two libraries owning one file means two answers to "where does this
    // appear". Refused with the existing one named, so the owner can act.
    let (pool, _catalog, _dir) = fixtures().await;
    let handler = RegisterLibraryHandler::new(
        SqliteLibraryRepository::new(pool.clone()),
        AlwaysStartsRun,
        LibraryRoot::unconfigured(),
    );
    handler
        .register("Course", ROOT, RunPriority::Normal)
        .await
        .map(|registered| registered.library)
        .expect("register");

    let nested = handler
        .register("Week one", "/library/course/class-01", RunPriority::Normal)
        .await
        .map(|registered| registered.library);

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
        SqliteLibraryRepository::new(pool.clone()),
        AlwaysStartsRun,
        LibraryRoot::unconfigured(),
    );
    handler
        .register("Course", ROOT, RunPriority::Normal)
        .await
        .map(|registered| registered.library)
        .expect("first");

    let sibling = handler
        .register("Notes", "/library/course-notes", RunPriority::Normal)
        .await
        .map(|registered| registered.library);

    assert!(sibling.is_ok(), "a sibling folder was refused: {sibling:?}");
}

#[tokio::test]
async fn given_a_padded_path_when_registered_then_the_overlap_is_still_refused() {
    // The check ran on what the caller sent and the insert stored the
    // trimmed form, so a path with leading spaces was compared as a prefix
    // of nothing and then stored as a folder genuinely inside another
    // library — the one state the design rules out.
    let (pool, _catalog, _dir) = fixtures().await;
    let handler = RegisterLibraryHandler::new(
        SqliteLibraryRepository::new(pool.clone()),
        AlwaysStartsRun,
        LibraryRoot::unconfigured(),
    );
    handler
        .register("Course", ROOT, RunPriority::Normal)
        .await
        .map(|registered| registered.library)
        .expect("register");

    let nested = handler
        .register(
            "Week one",
            "  /library/course/class-01  ",
            RunPriority::Normal,
        )
        .await
        .map(|registered| registered.library);

    match nested {
        Err(DomainError::Conflict(message)) => assert!(message.contains("Course"), "{message}"),
        other => panic!("expected a conflict naming the existing library, got {other:?}"),
    }
}

#[tokio::test]
async fn given_a_library_when_a_file_is_indexed_into_it_later_then_it_is_claimed_at_once() {
    // Registration claims what is already there; this is the other half. A
    // folder indexed again — a new class added, a re-scan after a download —
    // must not leave its files in the type panels until somebody thinks to
    // re-register the library.
    //
    // Resolved by the insert itself rather than by a sweep afterwards, so no
    // future index path can forget to run it.
    let (pool, catalog, _dir) = fixtures().await;

    let library = RegisterLibraryHandler::new(
        SqliteLibraryRepository::new(pool.clone()),
        AlwaysStartsRun,
        LibraryRoot::unconfigured(),
    )
    .register("Course", ROOT, RunPriority::Normal)
    .await
    .map(|registered| registered.library)
    .expect("register");

    // Indexed after the library existed.
    insert(
        &catalog,
        "/library/course/class-09/lecture.mp4",
        FileType::Video,
    )
    .await;

    let listing = BrowseLibraryHandler::new(
        SqliteLibraryRepository::new(pool.clone()),
        SqliteCatalogRepository::new(pool.clone()),
    )
    .browse(library.uuid, "")
    .await
    .expect("browse");
    assert_eq!(
        listing.folders.len(),
        1,
        "the new class was not in the tree"
    );
}

/// Windows paths, on a Linux runner (IR-01 targets both).
///
/// Every other test here spells its fixtures in POSIX, so all of them passed
/// while the feature claimed nothing at all on Windows: a root of
/// `D:\course` had `/` appended to it and matched no file in the catalog, and
/// the level below the top arrived at `level_of` as one unsplittable name.
/// The separator is the subject, so the paths are the fixture — there is no
/// need for a Windows runner to say this much.
mod windows_paths {
    use super::*;

    const WINDOWS_ROOT: &str = r"D:\courses\rust";

    async fn library_over(pool: &sqlx::sqlite::SqlitePool) -> Uuid {
        RegisterLibraryHandler::new(
            SqliteLibraryRepository::new(pool.clone()),
            AlwaysStartsRun,
            LibraryRoot::unconfigured(),
        )
        .register("Rust course", WINDOWS_ROOT, RunPriority::Normal)
        .await
        .map(|registered| registered.library)
        .expect("register")
        .uuid
    }

    async fn browse_at(pool: &sqlx::sqlite::SqlitePool, uuid: Uuid, path: &str) -> LibraryListing {
        BrowseLibraryHandler::new(
            SqliteLibraryRepository::new(pool.clone()),
            SqliteCatalogRepository::new(pool.clone()),
        )
        .browse(uuid, path)
        .await
        .expect("browse")
    }

    #[tokio::test]
    async fn given_a_windows_library_when_it_is_browsed_then_its_folders_are_seen() {
        let (pool, catalog, _dir) = fixtures().await;
        let uuid = library_over(&pool).await;
        insert(
            &catalog,
            r"D:\courses\rust\class-01\lecture.mp4",
            FileType::Video,
        )
        .await;
        insert(
            &catalog,
            r"D:\courses\rust\syllabus.pdf",
            FileType::Document,
        )
        .await;

        let top = browse_at(&pool, uuid, "").await;

        assert_eq!(
            top.folders
                .iter()
                .map(|f| f.name.clone())
                .collect::<Vec<_>>(),
            vec!["class-01"],
            "the class folder was invisible: the level arrived unsplit"
        );
        assert_eq!(top.files.len(), 1, "the syllabus was not at the top");

        // Addressed with the separator the caller happens to use: a client on
        // Windows sends what it read back from its own filesystem.
        let inside = browse_at(&pool, uuid, r"class-01").await;
        assert_eq!(inside.files.len(), 1, "the lecture was not in its class");
        assert!(inside.folders.is_empty());
    }

    #[tokio::test]
    async fn given_a_windows_library_when_a_sibling_is_registered_then_the_overlap_is_refused() {
        // The containment test reads the same roots, so it was wrong the same
        // way — and its failure is the dangerous direction: two libraries
        // claiming the same files, which the refusal exists to prevent.
        let (pool, _catalog, _dir) = fixtures().await;
        library_over(&pool).await;

        let nested = RegisterLibraryHandler::new(
            SqliteLibraryRepository::new(pool.clone()),
            AlwaysStartsRun,
            LibraryRoot::unconfigured(),
        )
        .register(
            "One class",
            r"D:\courses\rust\class-01",
            RunPriority::Normal,
        )
        .await
        .map(|registered| registered.library);

        assert!(
            matches!(nested, Err(DomainError::Conflict(_))),
            "a folder inside a library was registered as a second library"
        );
    }
}

/// Correcting a library's root when its folder moved (design section 1,
/// FR-FC-41).
mod moving {
    use super::*;
    use alexandria_core::libraries::commands::MoveLibraryHandler;

    async fn registered(pool: &sqlx::sqlite::SqlitePool, root: &str) -> Uuid {
        RegisterLibraryHandler::new(
            SqliteLibraryRepository::new(pool.clone()),
            AlwaysStartsRun,
            LibraryRoot::unconfigured(),
        )
        .register("Course", root, RunPriority::Normal)
        .await
        .map(|registered| registered.library)
        .expect("register")
        .uuid
    }

    async fn move_to(
        pool: &sqlx::sqlite::SqlitePool,
        uuid: Uuid,
        root: &str,
    ) -> Result<alexandria_core::libraries::model::Library, DomainError> {
        MoveLibraryHandler::new(
            SqliteLibraryRepository::new(pool.clone()),
            alexandria_core::catalog::library_root::LibraryRoot::unconfigured(),
            SqliteCatalogRunRepository::new(pool.clone()),
        )
        .move_to(uuid, root)
        .await
    }

    #[tokio::test]
    async fn given_a_library_when_moved_to_the_root_of_a_drive_then_the_root_keeps_its_separator() {
        // `E:` is not the root of drive E on Windows — it is the current
        // directory there — so a walk of a library stored as `E:` would find
        // `E:music`, not `E:\music`, and re-catalogue everything beside the
        // moved records. The paths below are rewritten without doubling it.
        let (pool, catalog, _dir) = fixtures().await;
        let uuid = registered(&pool, r"D:\courses").await;
        insert(&catalog, r"D:\courses\a.mp3", FileType::Audio).await;

        let moved = move_to(&pool, uuid, r"E:\").await.expect("move");

        assert_eq!(moved.root_path, r"E:\");
        let path: String = sqlx::query_scalar("SELECT path FROM files")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(path, r"E:\a.mp3");
    }

    #[tokio::test]
    async fn given_a_library_when_moved_to_the_filesystem_root_then_it_is_stored_as_slash() {
        let (pool, catalog, _dir) = fixtures().await;
        let uuid = registered(&pool, "/media/course").await;
        insert(&catalog, "/media/course/a.md", FileType::Text).await;

        let moved = move_to(&pool, uuid, "/").await.expect("move");

        assert_eq!(moved.root_path, "/");
        let path: String = sqlx::query_scalar("SELECT path FROM files")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(path, "/a.md");
    }

    async fn browse_at(pool: &sqlx::sqlite::SqlitePool, uuid: Uuid, path: &str) -> LibraryListing {
        BrowseLibraryHandler::new(
            SqliteLibraryRepository::new(pool.clone()),
            SqliteCatalogRepository::new(pool.clone()),
        )
        .browse(uuid, path)
        .await
        .expect("browse")
    }

    #[tokio::test]
    async fn given_a_moved_folder_when_the_root_is_corrected_then_its_files_follow() {
        // The whole promise: one row to correct rather than a re-index. If
        // the paths stayed behind, the library would browse as empty and the
        // records would all go missing at the next scan.
        let (pool, catalog, _dir) = fixtures().await;
        let uuid = registered(&pool, ROOT).await;
        insert(
            &catalog,
            "/library/course/class-01/lecture.mp4",
            FileType::Video,
        )
        .await;
        insert(&catalog, "/library/course/syllabus.pdf", FileType::Document).await;

        let moved = move_to(&pool, uuid, "/media/courses/rust")
            .await
            .expect("move");

        assert_eq!(moved.root_path, "/media/courses/rust");

        let top = browse_at(&pool, uuid, "").await;
        assert_eq!(
            top.folders
                .iter()
                .map(|f| f.name.clone())
                .collect::<Vec<_>>(),
            vec!["class-01"],
            "the tree did not survive the move"
        );
        assert_eq!(
            top.files[0].file.path, "/media/courses/rust/syllabus.pdf",
            "the file kept its old path"
        );
    }

    #[tokio::test]
    async fn given_a_root_with_non_ascii_letters_when_the_library_moves_then_paths_stay_whole() {
        // SQLite's `substr` counts characters, not bytes. The old root's
        // length was measured in bytes on the Rust side, so every multi-byte
        // letter in it sliced one character too many off each path below:
        // `/library/Música/class-01/lecture.mp4` moved to
        // `/media/rustclass-01/lecture.mp4` — the separator gone, and the
        // file's record pointing at a path that does not exist.
        let (pool, catalog, _dir) = fixtures().await;
        let uuid = registered(&pool, "/library/Música").await;
        insert(
            &catalog,
            "/library/Música/class-01/lecture.mp4",
            FileType::Video,
        )
        .await;
        insert(&catalog, "/library/Música/syllabus.pdf", FileType::Document).await;

        move_to(&pool, uuid, "/media/rust").await.expect("move");

        let top = browse_at(&pool, uuid, "").await;
        assert_eq!(
            top.files[0].file.path, "/media/rust/syllabus.pdf",
            "the path below the root was cut short"
        );
        let class = browse_at(&pool, uuid, "class-01").await;
        assert_eq!(class.files[0].file.path, "/media/rust/class-01/lecture.mp4");
    }

    #[tokio::test]
    async fn given_a_moved_library_when_its_files_are_read_then_they_are_the_same_records() {
        // What a re-index would cost, stated as an assertion: the uuid is
        // what a watchlist, a reading position and a collection all point
        // at, and a move must not mint a new one.
        let (pool, catalog, _dir) = fixtures().await;
        let uuid = registered(&pool, ROOT).await;
        let file = insert(&catalog, "/library/course/syllabus.pdf", FileType::Document).await;

        move_to(&pool, uuid, "/media/courses/rust")
            .await
            .expect("move");

        let top = browse_at(&pool, uuid, "").await;
        assert_eq!(top.files[0].file.uuid, file, "the record was replaced");
    }

    #[tokio::test]
    async fn given_a_windows_library_when_it_moves_then_the_paths_below_keep_their_separators() {
        // The root is replaced wholesale and everything below it is kept
        // exactly as indexed — which is what makes the slice safe on a
        // platform whose separator is not the one the comparison uses.
        let (pool, catalog, _dir) = fixtures().await;
        let uuid = registered(&pool, r"D:\courses\rust").await;
        insert(
            &catalog,
            r"D:\courses\rust\class-01\lecture.mp4",
            FileType::Video,
        )
        .await;

        move_to(&pool, uuid, r"E:\media\rust").await.expect("move");

        let inside = browse_at(&pool, uuid, "class-01").await;
        assert_eq!(
            inside.files[0].file.path,
            r"E:\media\rust\class-01\lecture.mp4"
        );
    }

    #[tokio::test]
    async fn given_a_library_when_it_moves_beneath_another_then_it_is_refused_by_name() {
        let (pool, _catalog, _dir) = fixtures().await;
        let uuid = registered(&pool, ROOT).await;
        RegisterLibraryHandler::new(
            SqliteLibraryRepository::new(pool.clone()),
            AlwaysStartsRun,
            LibraryRoot::unconfigured(),
        )
        .register("Photography", "/library/photos", RunPriority::Normal)
        .await
        .map(|registered| registered.library)
        .expect("register the other");

        let refused = move_to(&pool, uuid, "/library/photos/courses").await;

        assert!(
            matches!(refused, Err(DomainError::Conflict(_))),
            "a library was moved inside another one"
        );
    }

    #[tokio::test]
    async fn given_a_library_when_it_moves_within_its_own_root_then_it_is_allowed() {
        // A library always overlaps where it already is. Without excluding
        // itself from that question, no library could ever move at all — and
        // moving one level down is the ordinary case of a folder that was
        // registered a level too high.
        let (pool, catalog, _dir) = fixtures().await;
        let uuid = registered(&pool, ROOT).await;
        insert(
            &catalog,
            "/library/course/class-01/lecture.mp4",
            FileType::Video,
        )
        .await;

        let moved = move_to(&pool, uuid, "/library/course/class-01").await;

        assert!(moved.is_ok(), "a library could not be moved within itself");
    }

    #[tokio::test]
    async fn given_a_library_that_does_not_exist_when_moved_then_it_is_not_found() {
        let (pool, _catalog, _dir) = fixtures().await;

        let missing = move_to(&pool, Uuid::new_v4(), "/media/courses/rust").await;

        assert!(matches!(missing, Err(DomainError::NotFound)));
    }

    #[tokio::test]
    async fn given_a_blank_root_when_a_library_is_moved_then_it_is_rejected() {
        let (pool, _catalog, _dir) = fixtures().await;
        let uuid = registered(&pool, ROOT).await;

        let rejected = move_to(&pool, uuid, "   ").await;

        assert!(matches!(rejected, Err(DomainError::InvalidInput(_))));
    }

    // ---- FR-FC-26: the new root must sit inside `filesystem.root` ----

    async fn move_bounded(
        pool: &sqlx::sqlite::SqlitePool,
        uuid: Uuid,
        root: &str,
        library_root: &std::path::Path,
    ) -> Result<alexandria_core::libraries::model::Library, DomainError> {
        MoveLibraryHandler::new(
            SqliteLibraryRepository::new(pool.clone()),
            alexandria_core::catalog::library_root::LibraryRoot::new(
                library_root.to_str().unwrap(),
            ),
            SqliteCatalogRunRepository::new(pool.clone()),
        )
        .move_to(uuid, root)
        .await
    }

    #[tokio::test]
    async fn given_a_configured_root_when_a_library_moves_outside_it_then_it_is_refused_and_nothing_moves(
    ) {
        // The review's D1: a move rewrites every path without walking the
        // disk, so an unbounded move put paths outside the root in the catalog.
        let (pool, catalog, _dir) = fixtures().await;
        let parent = tempfile::tempdir().expect("tempdir");
        let library = parent.path().join("library");
        std::fs::create_dir(&library).unwrap();
        let root = library.join("x");
        let root = root.to_str().unwrap();
        let uuid = registered(&pool, root).await;
        insert(&catalog, &format!("{root}/passwd"), FileType::Document).await;

        let refused = move_bounded(&pool, uuid, "/etc", &library).await;

        assert!(
            matches!(refused, Err(DomainError::InvalidInput(ref m)) if m == "root path is outside the configured library root")
        );
        let top = browse_at(&pool, uuid, "").await;
        assert_eq!(top.files[0].file.path, format!("{root}/passwd"));
    }

    #[tokio::test]
    async fn given_a_configured_root_when_a_library_moves_to_a_missing_folder_inside_then_its_files_follow(
    ) {
        // The unplugged drive: nothing at the destination exists yet, but
        // its nearest existing ancestor is the library root.
        let (pool, catalog, _dir) = fixtures().await;
        let library = tempfile::tempdir().expect("tempdir");
        let root = library.path().join("course");
        let root = root.to_str().unwrap();
        let uuid = registered(&pool, root).await;
        insert(
            &catalog,
            &format!("{root}/syllabus.pdf"),
            FileType::Document,
        )
        .await;
        let destination = library.path().join("usb").join("rust");
        let destination = destination.to_str().unwrap();

        move_bounded(&pool, uuid, destination, library.path())
            .await
            .expect("move");

        let top = browse_at(&pool, uuid, "").await;
        assert_eq!(
            top.files[0].file.path,
            format!("{destination}/syllabus.pdf")
        );
    }
}

/// A folder whose name differs from a library's only in case.
///
/// `LIKE` is case-insensitive over ASCII, and these three queries used it —
/// the claim at registration, the claim at insert time, and the overlap
/// check. On every filesystem this ships to but Windows's, `/library/Photos`
/// and `/library/photos` are two different folders, so the case-blind
/// comparison was three separate wrongs: files claimed by a library they are
/// not under, files claimed at the next scan, and a legitimate second folder
/// refused as an overlap.
///
/// The claim is the worst of the three, and the reason these are not merely
/// tidiness. `BrowseLibraryHandler` strips the root with Rust's
/// `strip_prefix`, which *is* case-sensitive, so a wrongly claimed file fails
/// to strip and is dropped from the tree — while `library_id` keeps it out of
/// its type panel. The owner's file is then reachable from nowhere but
/// search.
mod case {
    use super::*;

    #[tokio::test]
    async fn given_a_folder_differing_only_in_case_when_registered_then_it_is_not_an_overlap() {
        // The mirror image, and the one the owner meets first: two real
        // folders, and the second refused with "that folder overlaps the
        // library Photos" — a message about a folder they are not in.
        let (pool, _catalog, _dir) = fixtures().await;
        RegisterLibraryHandler::new(
            SqliteLibraryRepository::new(pool.clone()),
            AlwaysStartsRun,
            LibraryRoot::unconfigured(),
        )
        .register("Photos", "/library/photos", RunPriority::Normal)
        .await
        .map(|registered| registered.library)
        .expect("register");

        RegisterLibraryHandler::new(
            SqliteLibraryRepository::new(pool.clone()),
            AlwaysStartsRun,
            LibraryRoot::unconfigured(),
        )
        .register("Photographs", "/library/Photos", RunPriority::Normal)
        .await
        .map(|registered| registered.library)
        .expect("a folder differing only in case was refused as an overlap");
    }

    #[tokio::test]
    async fn given_the_same_folder_when_registered_twice_then_it_is_still_an_overlap() {
        // The guard on the guard: making the comparison case-sensitive must
        // not make it stop catching the case it exists for.
        let (pool, _catalog, _dir) = fixtures().await;
        RegisterLibraryHandler::new(
            SqliteLibraryRepository::new(pool.clone()),
            AlwaysStartsRun,
            LibraryRoot::unconfigured(),
        )
        .register("Photos", "/library/photos", RunPriority::Normal)
        .await
        .map(|registered| registered.library)
        .expect("register");

        let refused = RegisterLibraryHandler::new(
            SqliteLibraryRepository::new(pool.clone()),
            AlwaysStartsRun,
            LibraryRoot::unconfigured(),
        )
        .register("Inner", "/library/photos/2024", RunPriority::Normal)
        .await
        .map(|registered| registered.library);

        assert!(
            matches!(refused, Err(DomainError::Conflict(_))),
            "a folder inside a library was not refused: {refused:?}"
        );
    }
}
