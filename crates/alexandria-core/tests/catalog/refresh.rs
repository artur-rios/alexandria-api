use std::sync::Arc;

use uuid::Uuid;

use alexandria_core::auth::AuthService;
use alexandria_core::catalog::clock::Clock;
use alexandria_core::catalog::commands::refresh::{RefreshHandler, RefreshStarted};
use alexandria_core::catalog::commands::run_control::RunControlHandler;
use alexandria_core::catalog::fs::Filesystem;
use alexandria_core::catalog::model::{
    File, FileType, MediaKind, NewFile, SubtypeMetadata, METADATA_VERSION,
};
use alexandria_core::catalog::repos::{CatalogRepository, SqliteCatalogRepository};
use alexandria_core::catalog::run_registry::{RunPhase, RunRegistry};
use alexandria_core::catalog::runs::{
    CatalogRunRepository, RunCounts, RunKind, RunPriority, RunStatus,
};
use alexandria_core::catalog::video_tags::VideoTags;
use alexandria_core::errors::DomainError;
use alexandria_core::migrate::run_migrations;
use sqlx::sqlite::SqlitePoolOptions;

use crate::common::{
    a_cataloged_file, a_cataloged_file_with_hash, a_cataloged_missing_file, fixed_clock, interrupt,
    now, FailingCatalogRepository, FailingCatalogRunRepository, FakeAuth, FakeCatalogRepository,
    FakeCatalogRunRepository, FakeComicMetadataReader, FakeDocumentMetadataReader, FakeFilesystem,
    FakeImageMetadataReader, FakeVideoMetadataReader, InterruptingFilesystem, Seam,
};

const TOKEN: &str = "bearer-token";

/// Deliberately > 1 so these tests exercise the concurrent walk — the outcome
/// tallies must not depend on how many paths are in flight.
const TEST_CONCURRENCY: u32 = 4;

/// The low-priority width these unit tests build handlers with. Deliberately
/// distinct from [`TEST_CONCURRENCY`], mirroring `tests/catalog/index.rs`.
const TEST_LOW_PRIORITY_CONCURRENCY: u32 = 1;

/// The four readers a refresh now holds, seeded with nothing.
///
/// A reader that answers `None` for every path is a file whose tags will not
/// parse, which is what these tests want: they are about stat comparison and
/// run control, and a refresh only re-reads a file whose row is behind the
/// current extraction. The tests that *are* about that re-reading seed a
/// reader of their own — see `metadata backfill`.
type TestReaders = (
    FakeImageMetadataReader,
    FakeDocumentMetadataReader,
    FakeVideoMetadataReader,
    FakeComicMetadataReader,
);

fn no_readers() -> TestReaders {
    (
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
    )
}

fn refresh_handler<A, R, F, C, RR>(
    auth: A,
    repo: R,
    fs: F,
    clock: C,
    runs: RR,
) -> RefreshHandler<
    A,
    R,
    F,
    C,
    RR,
    FakeImageMetadataReader,
    FakeDocumentMetadataReader,
    FakeVideoMetadataReader,
    FakeComicMetadataReader,
>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
    C: Clock,
    RR: CatalogRunRepository,
{
    let (image, document, video, comic) = no_readers();
    RefreshHandler::new(
        auth,
        repo,
        fs,
        clock,
        image,
        document,
        video,
        comic,
        TEST_CONCURRENCY,
        TEST_LOW_PRIORITY_CONCURRENCY,
        runs,
        // Progress goes somewhere no test reads; the ones that do read it
        // build the handler with their own registry.
        RunRegistry::new(),
    )
}

#[tokio::test]
async fn given_authenticated_when_refresh_start_then_returns_run_id() {
    let fs = FakeFilesystem::builder().build();
    let handler = refresh_handler(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        fs,
        fixed_clock(now()),
        FakeCatalogRunRepository::new(),
    );

    let started: RefreshStarted = handler
        .start(RunPriority::Normal, TOKEN)
        .await
        .expect("start");
    assert_ne!(started.run_id, Uuid::nil());
}

/// Closes a gap Task 9's review found: every other test in this file starts
/// at `RunPriority::Normal`, so a swapped `match` in
/// `RefreshHandler::concurrency_for`'s `Low` arm would ship undetected. Mirror
/// of `index.rs`'s
/// `given_a_low_priority_index_when_started_then_the_run_records_the_low_concurrency`.
#[tokio::test]
async fn given_a_low_priority_refresh_when_started_then_the_run_records_the_low_concurrency() {
    let runs = FakeCatalogRunRepository::new();
    let fs = FakeFilesystem::builder().build();
    let handler = refresh_handler(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        fs,
        fixed_clock(now()),
        runs.clone(),
    );

    let started: RefreshStarted = handler.start(RunPriority::Low, TOKEN).await.expect("start");

    assert_eq!(
        runs.get_recorded(started.run_id).unwrap().concurrency,
        Some(TEST_LOW_PRIORITY_CONCURRENCY)
    );
}

#[tokio::test]
async fn given_unauthenticated_when_refresh_start_then_unauthorized() {
    let fs = FakeFilesystem::builder().build();
    let handler = refresh_handler(
        FakeAuth::Denying,
        FakeCatalogRepository::new(),
        fs,
        fixed_clock(now()),
        FakeCatalogRunRepository::new(),
    );

    let result = handler.start(RunPriority::Normal, "").await;
    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

// ---------------- Metadata backfill (UC-02) ----------------

/// A handler whose video reader is the caller's, so a test can say what the
/// file's tags are.
fn refresh_handler_reading(
    repo: FakeCatalogRepository,
    fs: FakeFilesystem,
    video: FakeVideoMetadataReader,
) -> RefreshHandler<
    FakeAuth,
    FakeCatalogRepository,
    FakeFilesystem,
    impl Clock,
    FakeCatalogRunRepository,
    FakeImageMetadataReader,
    FakeDocumentMetadataReader,
    FakeVideoMetadataReader,
    FakeComicMetadataReader,
> {
    RefreshHandler::new(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        video,
        FakeComicMetadataReader::new(),
        TEST_CONCURRENCY,
        TEST_LOW_PRIORITY_CONCURRENCY,
        FakeCatalogRunRepository::new(),
        RunRegistry::new(),
    )
}

/// A catalogued video with explicit stats, behind the current extraction.
fn a_cataloged_video(path: &str, size_bytes: i64) -> File {
    File {
        file_type: FileType::Video,
        ..a_cataloged_file(path, size_bytes, Some(now()))
    }
}

fn tags_with_a_year() -> VideoTags {
    VideoTags {
        title: Some("Stalker".to_string()),
        year: Some(1979),
        resolution: Some("1920x1080".to_string()),
        duration_seconds: None,
    }
}

/// A row written before the extractor could read a field keeps NULL there
/// forever, because a re-index skips a catalogued path and a refresh only
/// compares stat. The file has not changed — the catalog's reading of it
/// has.
#[tokio::test]
async fn given_a_row_an_older_extraction_wrote_when_refreshed_then_its_metadata_is_filled() {
    let repo = FakeCatalogRepository::new();
    let file = a_cataloged_video("/library/film.mkv", 4096);
    let uuid = file.uuid;
    repo.seed(file);
    let repo_handle = repo.clone();

    let video = FakeVideoMetadataReader::new();
    video.seed("/library/film.mkv", tags_with_a_year());

    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/library/film.mkv", "film.mkv", "unused")
        .with_stat("/library/film.mkv", 4096, Some(now()))
        .build();

    let outcome = refresh_handler_reading(repo, fs, video)
        .execute(Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.metadata_filled, 1);
    assert_eq!(
        outcome.unchanged, 0,
        "a row that was re-read is not an untouched one"
    );
    assert_eq!(
        repo_handle
            .metadata_for(uuid)
            .and_then(|metadata| match metadata {
                SubtypeMetadata::Video { year, .. } => year,
                _ => None,
            }),
        Some(1979),
        "the tag was in the file all along; the catalog had never read it"
    );
}

/// A file whose bytes changed is re-read too, and for a better reason: its
/// tags may have changed with them. It stays counted as refreshed — that the
/// file itself moved on is the larger fact about that path.
#[tokio::test]
async fn given_a_changed_file_when_refreshed_then_its_metadata_is_read_as_well() {
    let repo = FakeCatalogRepository::new();
    let mut file = a_cataloged_video("/library/retagged.mkv", 4096);
    // Stamped current: the row is not behind, and the file is read anyway.
    // The stamp records which extraction last read a row, not which bytes it
    // read — a retagged file has new bytes and the same stamp.
    file.metadata_version = METADATA_VERSION;
    let uuid = file.uuid;
    repo.seed(file);
    let repo_handle = repo.clone();

    let video = FakeVideoMetadataReader::new();
    video.seed("/library/retagged.mkv", tags_with_a_year());

    // A different size: the stat comparison calls this changed.
    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/library/retagged.mkv", "retagged.mkv", "unused")
        .with_stat("/library/retagged.mkv", 8192, Some(now()))
        .build();

    let outcome = refresh_handler_reading(repo, fs, video)
        .execute(Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.refreshed, 1);
    assert_eq!(
        outcome.metadata_filled, 0,
        "the path is reported by what happened to the file, not to its row"
    );
    assert!(
        repo_handle.metadata_for(uuid).is_some(),
        "a file whose bytes changed had its tags read too"
    );
}

/// Once, not once a run. The stamp is the whole reason a refresh can afford
/// to read tags at all.
#[tokio::test]
async fn given_a_row_at_the_current_version_when_refreshed_then_the_file_is_not_read() {
    let repo = FakeCatalogRepository::new();
    let mut file = a_cataloged_video("/library/current.mkv", 4096);
    file.metadata_version = METADATA_VERSION;
    repo.seed(file);

    let video = FakeVideoMetadataReader::new();
    video.seed("/library/current.mkv", tags_with_a_year());
    let video_handle = video.clone();

    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/library/current.mkv", "current.mkv", "unused")
        .with_stat("/library/current.mkv", 4096, Some(now()))
        .build();

    let outcome = refresh_handler_reading(repo, fs, video)
        .execute(Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.unchanged, 1);
    assert_eq!(outcome.metadata_filled, 0);
    assert_eq!(
        video_handle.call_count(),
        0,
        "a row already current must cost no read at all"
    );
}

/// A file whose tags will not parse is still stamped: the stamp records
/// which extraction has been applied, and a file that gave up nothing has
/// had this one applied as fully as it ever will be — so it is not opened
/// again on every later pass.
#[tokio::test]
async fn given_tags_that_will_not_parse_when_refreshed_then_the_row_is_left_behind() {
    let repo = FakeCatalogRepository::new();
    repo.seed(a_cataloged_video("/library/damaged.mkv", 4096));
    let repo_handle = repo.clone();

    // Nothing seeded: the reader answers `None`, exactly as the real one
    // does for a file it cannot parse.
    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/library/damaged.mkv", "damaged.mkv", "unused")
        .with_stat("/library/damaged.mkv", 4096, Some(now()))
        .build();

    let outcome = refresh_handler_reading(repo, fs, FakeVideoMetadataReader::new())
        .execute(Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(
        outcome.unchanged, 1,
        "nothing was read, so nothing was filled"
    );
    assert_eq!(outcome.metadata_filled, 0);
    assert_eq!(
        repo_handle
            .file_for("/library/damaged.mkv")
            .unwrap()
            .metadata_version,
        METADATA_VERSION,
        "the row must not be opened again on every later pass"
    );
}

/// The refresh fills; it does not replace. The repository enforces this
/// column by column (`fill_missing_metadata`, pinned against real SQLite
/// below); what this pins is that the *command* asks for the filling one
/// rather than the replacing one.
#[tokio::test]
async fn given_metadata_the_owner_edited_when_refreshed_then_it_is_not_overwritten() {
    let repo = FakeCatalogRepository::new();
    let file = a_cataloged_video("/library/corrected.mkv", 4096);
    let uuid = file.uuid;
    repo.seed(file);
    repo.seed_metadata(
        uuid,
        SubtypeMetadata::Video {
            title: Some("The Owner's Title".to_string()),
            year: None,
            resolution: None,
            media_kind: None,
        },
    );
    let repo_handle = repo.clone();

    let video = FakeVideoMetadataReader::new();
    video.seed("/library/corrected.mkv", tags_with_a_year());

    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/library/corrected.mkv", "corrected.mkv", "unused")
        .with_stat("/library/corrected.mkv", 4096, Some(now()))
        .build();

    refresh_handler_reading(repo, fs, video)
        .execute(Uuid::new_v4())
        .await
        .expect("execute");

    assert!(
        matches!(
            repo_handle.metadata_for(uuid),
            Some(SubtypeMetadata::Video { title: Some(title), .. }) if title == "The Owner's Title"
        ),
        "a title the owner corrected must outrank whatever the tag says"
    );
}

/// A full replace writes `None` as NULL, which is why the repair cannot be
/// one: no file carries a video's media kind, so a replace would blank the
/// answer the owner gave (FR-ME-02). Pinned against a real migrated SQLite
/// database, where `fill_missing_metadata`'s column-by-column `COALESCE`
/// actually runs.
#[tokio::test]
async fn given_a_media_kind_the_owner_set_when_filled_then_it_survives() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");
    run_migrations(&pool).await.expect("migrate");
    let repo = SqliteCatalogRepository::new(pool);
    let uuid = Uuid::new_v4();
    repo.insert_file(NewFile {
        uuid,
        path: "/library/film.mkv".to_string(),
        name: "film.mkv".to_string(),
        file_type: FileType::Video,
        content_hash: None,
        size_bytes: None,
        mtime: None,
        indexed_at: chrono::Utc::now(),
    })
    .await
    .expect("insert file");
    repo.update_metadata(
        uuid,
        &SubtypeMetadata::Video {
            title: None,
            year: None,
            resolution: None,
            media_kind: Some(MediaKind::Series),
        },
    )
    .await
    .expect("seed the owner's own answer");

    repo.fill_missing_metadata(
        uuid,
        &SubtypeMetadata::Video {
            title: Some("Stalker".to_string()),
            year: Some(1979),
            resolution: None,
            media_kind: None,
        },
    )
    .await
    .expect("fill");

    let view = repo.find_metadata_by_uuid(uuid).await.expect("read");
    assert_eq!(
        view,
        Some(SubtypeMetadata::Video {
            title: Some("Stalker".to_string()),
            year: Some(1979),
            resolution: None,
            media_kind: Some(MediaKind::Series),
        })
    );
}

// ---------------- Stat comparison (Task 4 / FR-FC-10) ----------------

#[tokio::test]
async fn given_an_unchanged_file_when_refreshed_then_it_is_unchanged_and_no_bytes_are_read() {
    let repo = FakeCatalogRepository::new();
    repo.seed(a_cataloged_file("/library/notes.txt", 4096, Some(now())));
    let repo_handle = repo.clone();

    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/library/notes.txt", "notes.txt", "unused")
        .with_stat("/library/notes.txt", 4096, Some(now()))
        .build();
    let handler = refresh_handler(
        FakeAuth::Allowing,
        repo,
        fs.clone(),
        fixed_clock(now()),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler.execute(Uuid::new_v4()).await.expect("execute");

    assert_eq!(outcome.unchanged, 1);
    assert_eq!(outcome.refreshed, 0);
    assert_eq!(fs.hash_calls(), 0, "refresh must not hash");
    assert!(
        repo_handle
            .file_for("/library/notes.txt")
            .unwrap()
            .content_hash
            .is_none(),
        "an unchanged file's hash (already None) is untouched"
    );
}

#[tokio::test]
async fn given_a_file_whose_size_changed_when_refreshed_then_it_is_refreshed_and_its_hash_is_cleared(
) {
    let repo = FakeCatalogRepository::new();
    repo.seed(a_cataloged_file_with_hash(
        "/library/notes.txt",
        4096,
        Some(now()),
        "abc",
    ));
    let repo_handle = repo.clone();

    // Same mtime, different size on disk: either one differing is a change.
    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/library/notes.txt", "notes.txt", "unused")
        .with_stat("/library/notes.txt", 8192, Some(now()))
        .build();
    let handler = refresh_handler(
        FakeAuth::Allowing,
        repo,
        fs.clone(),
        fixed_clock(now()),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler.execute(Uuid::new_v4()).await.expect("execute");

    assert_eq!(outcome.refreshed, 1);
    let file = repo_handle.file_for("/library/notes.txt").unwrap();
    assert_eq!(file.size_bytes, Some(8192));
    assert_eq!(
        file.content_hash, None,
        "a stale hash must not outlive the bytes"
    );
    assert_eq!(fs.hash_calls(), 0, "refresh must not hash");
}

#[tokio::test]
async fn given_a_file_whose_mtime_changed_when_refreshed_then_it_is_refreshed() {
    let repo = FakeCatalogRepository::new();
    repo.seed(a_cataloged_file_with_hash(
        "/library/notes.txt",
        4096,
        Some(now()),
        "abc",
    ));
    let repo_handle = repo.clone();

    // Same size, different mtime: either one differing is a change.
    let later = now() + chrono::Duration::seconds(1);
    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/library/notes.txt", "notes.txt", "unused")
        .with_stat("/library/notes.txt", 4096, Some(later))
        .build();
    let handler = refresh_handler(
        FakeAuth::Allowing,
        repo,
        fs.clone(),
        fixed_clock(now()),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler.execute(Uuid::new_v4()).await.expect("execute");

    assert_eq!(outcome.refreshed, 1);
    let file = repo_handle.file_for("/library/notes.txt").unwrap();
    assert_eq!(file.mtime, Some(later));
    assert_eq!(fs.hash_calls(), 0, "refresh must not hash");
}

#[tokio::test]
async fn given_disk_missing_path_when_execute_then_marked_missing_record_kept() {
    let repo = FakeCatalogRepository::new();
    repo.seed(a_cataloged_file_with_hash(
        "/library/gone.txt",
        4096,
        Some(now()),
        "old-hash",
    ));
    let repo_handle = repo.clone();

    // The file is NOT registered with the filesystem -> stat reports None.
    let fs = FakeFilesystem::builder().build();
    let handler = refresh_handler(
        FakeAuth::Allowing,
        repo,
        fs.clone(),
        fixed_clock(now()),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler.execute(Uuid::new_v4()).await.expect("execute");

    assert_eq!(outcome.marked_missing, 1);
    assert_eq!(outcome.refreshed, 0);
    assert_eq!(outcome.unchanged, 0);
    assert_eq!(fs.hash_calls(), 0, "refresh must not hash");

    let gone = repo_handle
        .file_for("/library/gone.txt")
        .expect("record kept");
    assert!(gone.missing_at.is_some(), "missing marker set");
    assert_eq!(
        gone.state,
        alexandria_core::catalog::model::FileState::Active,
        "state is NOT set to deleted (soft-delete is UC-06)"
    );
    assert_eq!(
        gone.content_hash,
        Some("old-hash".to_string()),
        "hash untouched when missing"
    );
}

#[tokio::test]
async fn given_missing_file_returned_on_disk_with_matching_stat_when_execute_then_missing_cleared_and_refreshed(
) {
    // Already marked missing from a prior re-index; the file has since come
    // back on disk with the SAME size/mtime it had before it vanished. Even
    // though the stat matches, `missing_at` still has to be cleared, so this
    // must count as `refreshed`, not `unchanged`.
    let repo = FakeCatalogRepository::new();
    repo.seed(a_cataloged_missing_file(
        "/library/back.txt",
        4096,
        Some(now()),
        "old-hash",
    ));
    let repo_handle = repo.clone();

    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/library/back.txt", "back.txt", "unused")
        .with_stat("/library/back.txt", 4096, Some(now()))
        .build();
    let handler = refresh_handler(
        FakeAuth::Allowing,
        repo,
        fs.clone(),
        fixed_clock(now()),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler.execute(Uuid::new_v4()).await.expect("execute");

    assert_eq!(outcome.refreshed, 1);
    assert_eq!(outcome.marked_missing, 0);
    assert_eq!(fs.hash_calls(), 0, "refresh must not hash");

    let back = repo_handle
        .file_for("/library/back.txt")
        .expect("record kept");
    assert_eq!(
        back.content_hash, None,
        "refresh_stat clears the hash even though the stat matched"
    );
    assert!(
        back.missing_at.is_none(),
        "missing marker cleared on return"
    );
    assert_eq!(back.indexed_at, now());
}

#[tokio::test]
async fn given_already_missing_and_still_gone_when_execute_then_left_as_is() {
    let repo = FakeCatalogRepository::new();
    repo.seed(a_cataloged_missing_file(
        "/library/stillgone.txt",
        4096,
        Some(now()),
        "old-hash",
    ));
    let repo_handle = repo.clone();
    let prior_missing = repo_handle
        .file_for("/library/stillgone.txt")
        .unwrap()
        .missing_at;

    let fs = FakeFilesystem::builder().build(); // file still absent
    let handler = refresh_handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler.execute(Uuid::new_v4()).await.expect("execute");

    assert_eq!(outcome.marked_missing, 0, "no new missing write");
    assert_eq!(outcome.unchanged, 1, "idempotent — left as-is");
    let gone = repo_handle.file_for("/library/stillgone.txt").unwrap();
    assert_eq!(
        gone.missing_at, prior_missing,
        "missing timestamp not bumped"
    );
}

#[tokio::test]
async fn given_a_repository_write_failure_when_execute_then_refresh_continues_and_counts_failure() {
    let repo = FakeCatalogRepository::new();
    repo.seed(a_cataloged_file_with_hash(
        "/library/a.txt",
        4096,
        Some(now()),
        "a-old",
    ));
    repo.seed(a_cataloged_file_with_hash(
        "/library/b.txt",
        4096,
        Some(now()),
        "b-old",
    ));
    let repo = repo.failing_for("/library/a.txt");
    let repo_handle = repo.clone();

    // Both files changed size on disk, so both would normally be refreshed.
    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/library/a.txt", "a.txt", "unused")
        .with_stat("/library/a.txt", 8192, Some(now()))
        .with_file("/lib", "/library/b.txt", "b.txt", "unused")
        .with_stat("/library/b.txt", 8192, Some(now()))
        .build();
    let handler = refresh_handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(Uuid::new_v4())
        .await
        .expect("a per-file repository error must not fail the whole refresh");

    assert_eq!(outcome.refreshed, 1);
    assert_eq!(outcome.failed, 1);
    assert_eq!(
        repo_handle.file_for("/library/b.txt").unwrap().size_bytes,
        Some(8192)
    );
    assert_eq!(
        repo_handle.file_for("/library/a.txt").unwrap().size_bytes,
        Some(4096),
        "the failed write leaves the prior stat in place"
    );
}

// ---------------- Bounded concurrency (FR-FC-08) ----------------

/// Paths are refreshed several at a time, so the visit order is unspecified —
/// the tallies must not be. Every concurrency from sequential to
/// wider-than-the-catalog produces the same counts over the same catalog, and
/// each of the four outcomes is represented so none of them can be
/// mis-attributed by the concurrent fold.
#[tokio::test]
async fn given_any_concurrency_when_execute_then_same_outcome_tallies() {
    for concurrency in [1u32, 2, 4, 16] {
        let repo = FakeCatalogRepository::new();
        // changed on disk -> refreshed
        repo.seed(a_cataloged_file_with_hash(
            "/library/a.txt",
            4096,
            Some(now()),
            "a-old",
        ));
        // same stat -> unchanged
        repo.seed(a_cataloged_file_with_hash(
            "/library/b.txt",
            4096,
            Some(now()),
            "b-same",
        ));
        // absent on disk -> marked missing
        repo.seed(a_cataloged_file_with_hash(
            "/library/c.txt",
            4096,
            Some(now()),
            "c-old",
        ));
        // present but the write fails -> failed
        repo.seed(a_cataloged_file_with_hash(
            "/library/d.txt",
            4096,
            Some(now()),
            "d-old",
        ));
        let repo = repo.failing_for("/library/d.txt");

        let fs = FakeFilesystem::builder()
            .with_file("/lib", "/library/a.txt", "a.txt", "unused")
            .with_stat("/library/a.txt", 8192, Some(now()))
            .with_file("/lib", "/library/b.txt", "b.txt", "unused")
            .with_stat("/library/b.txt", 4096, Some(now()))
            .with_file("/lib", "/library/d.txt", "d.txt", "unused")
            .with_stat("/library/d.txt", 8192, Some(now()))
            .build();
        let handler = RefreshHandler::new(
            FakeAuth::Allowing,
            repo,
            fs,
            fixed_clock(now()),
            FakeImageMetadataReader::new(),
            FakeDocumentMetadataReader::new(),
            FakeVideoMetadataReader::new(),
            FakeComicMetadataReader::new(),
            concurrency,
            TEST_LOW_PRIORITY_CONCURRENCY,
            FakeCatalogRunRepository::new(),
            RunRegistry::new(),
        );

        let outcome = handler.execute(Uuid::new_v4()).await.expect("execute");

        assert_eq!(outcome.refreshed, 1, "concurrency {concurrency}");
        assert_eq!(outcome.unchanged, 1, "concurrency {concurrency}");
        assert_eq!(outcome.marked_missing, 1, "concurrency {concurrency}");
        assert_eq!(outcome.failed, 1, "concurrency {concurrency}");
    }
}

/// Zero is clamped to sequential rather than buffering zero deep and hanging.
#[tokio::test]
async fn given_zero_concurrency_when_execute_then_runs_sequentially_rather_than_hanging() {
    let repo = FakeCatalogRepository::new();
    repo.seed(a_cataloged_file_with_hash(
        "/library/a.txt",
        4096,
        Some(now()),
        "a-old",
    ));
    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/library/a.txt", "a.txt", "unused")
        .with_stat("/library/a.txt", 8192, Some(now()))
        .build();
    let handler = RefreshHandler::new(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        0,
        TEST_LOW_PRIORITY_CONCURRENCY,
        FakeCatalogRunRepository::new(),
        RunRegistry::new(),
    );

    let outcome = handler.execute(Uuid::new_v4()).await.expect("execute");

    assert_eq!(outcome.refreshed, 1);
}

#[tokio::test]
async fn given_no_cataloged_files_when_execute_then_empty_outcome() {
    let repo = FakeCatalogRepository::new();
    let fs = FakeFilesystem::builder().build();
    let handler = refresh_handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler.execute(Uuid::new_v4()).await.expect("execute");
    assert_eq!(outcome.refreshed, 0);
    assert_eq!(outcome.marked_missing, 0);
    assert_eq!(outcome.unchanged, 0);
}

#[tokio::test]
async fn given_mixed_cataloged_files_when_execute_then_each_handled_correctly() {
    // A: changed -> refreshed; B: unchanged present; C: missing on disk.
    let repo = FakeCatalogRepository::new();
    repo.seed(a_cataloged_file_with_hash(
        "/library/a.txt",
        4096,
        Some(now()),
        "a-old",
    ));
    repo.seed(a_cataloged_file_with_hash(
        "/library/b.md",
        4096,
        Some(now()),
        "b-hash",
    ));
    repo.seed(a_cataloged_file_with_hash(
        "/library/c.pdf",
        4096,
        Some(now()),
        "c-hash",
    ));
    let repo_handle = repo.clone();

    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/library/a.txt", "a.txt", "unused")
        .with_stat("/library/a.txt", 8192, Some(now()))
        .with_file("/lib", "/library/b.md", "b.md", "unused")
        .with_stat("/library/b.md", 4096, Some(now()))
        // c.pdf absent on disk
        .build();
    let handler = refresh_handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler.execute(Uuid::new_v4()).await.expect("execute");

    assert_eq!(outcome.refreshed, 1);
    assert_eq!(outcome.marked_missing, 1);
    assert_eq!(outcome.unchanged, 1);

    assert_eq!(
        repo_handle.file_for("/library/a.txt").unwrap().content_hash,
        None,
        "refreshed file's hash is cleared"
    );
    assert_eq!(
        repo_handle.file_for("/library/b.md").unwrap().content_hash,
        Some("b-hash".to_string()),
        "unchanged file's hash is untouched"
    );
    assert!(repo_handle
        .file_for("/library/c.pdf")
        .unwrap()
        .missing_at
        .is_some());
}

// ---------------- Run record lifecycle (UC-42 / FR-FC-27) ----------------

#[tokio::test]
async fn given_a_started_refresh_when_started_then_the_run_is_recorded_running() {
    let runs = FakeCatalogRunRepository::new();
    let handler = refresh_handler(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        FakeFilesystem::builder().build(),
        fixed_clock(now()),
        runs.clone(),
    );

    let started = handler
        .start(RunPriority::Normal, TOKEN)
        .await
        .expect("start");

    let recorded = runs.get_recorded(started.run_id).expect("run recorded");
    assert_eq!(recorded.kind, RunKind::Refresh);
    assert_eq!(recorded.status, RunStatus::Running);
    assert!(recorded.root.is_none(), "a refresh takes no root");
}

#[tokio::test]
async fn given_a_refresh_that_walks_when_executed_then_the_run_is_recorded_complete() {
    let runs = FakeCatalogRunRepository::new();
    // Same fixture shape as the "size changed" stat test above: one cataloged
    // file whose on-disk size changed.
    let repo = FakeCatalogRepository::new();
    repo.seed(a_cataloged_file_with_hash(
        "/library/a.txt",
        4096,
        Some(now()),
        "old-hash",
    ));
    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/library/a.txt", "a.txt", "unused")
        .with_stat("/library/a.txt", 8192, Some(now()))
        .build();
    let handler = refresh_handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        runs.clone(),
    );

    let started = handler
        .start(RunPriority::Normal, TOKEN)
        .await
        .expect("start");
    let outcome = handler.execute(started.run_id).await.expect("execute");

    let recorded = runs.get_recorded(started.run_id).expect("run recorded");
    assert_eq!(recorded.status, RunStatus::Complete);
    assert_eq!(
        recorded.counts,
        Some(RunCounts::Refresh {
            refreshed: outcome.refreshed,
            marked_missing: outcome.marked_missing,
            unchanged: outcome.unchanged,
            failed: outcome.failed,
        }),
        "the recorded tally is the outcome the walk computed"
    );
}

#[tokio::test]
async fn given_a_catalog_that_cannot_be_listed_when_executed_then_the_run_is_recorded_failed() {
    // FR-FC-27: this is the only case that makes a run `failed` — the walk
    // could not proceed at all.
    let runs = FakeCatalogRunRepository::new();
    let handler = refresh_handler(
        FakeAuth::Allowing,
        FailingCatalogRepository,
        FakeFilesystem::builder().build(),
        fixed_clock(now()),
        runs.clone(),
    );

    let started = handler
        .start(RunPriority::Normal, TOKEN)
        .await
        .expect("start");
    let err = handler
        .execute(started.run_id)
        .await
        .expect_err("must fail");

    let recorded = runs.get_recorded(started.run_id).expect("run recorded");
    assert_eq!(recorded.status, RunStatus::Failed);
    assert!(
        recorded.error.is_some(),
        "a failed run carries the underlying error"
    );
    assert!(recorded.counts.is_none());
    let _ = err;
}

#[tokio::test]
async fn given_run_completion_cannot_be_recorded_when_executed_then_the_outcome_is_still_returned()
{
    // FR-FC-27: the walk itself succeeds; only the bookkeeping write fails.
    // The caller must still see the outcome it computed — a bookkeeping
    // failure must not sink a completed walk. Same single-file fixture as
    // above.
    let repo = FakeCatalogRepository::new();
    repo.seed(a_cataloged_file_with_hash(
        "/library/a.txt",
        4096,
        Some(now()),
        "old-hash",
    ));
    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/library/a.txt", "a.txt", "unused")
        .with_stat("/library/a.txt", 8192, Some(now()))
        .build();
    let handler = refresh_handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FailingCatalogRunRepository::FinishFails,
    );

    let outcome = handler
        .execute(Uuid::new_v4())
        .await
        .expect("a failed run-completion write must not fail the walk");

    assert_eq!(outcome.refreshed, 1);
    assert_eq!(outcome.marked_missing, 0);
    assert_eq!(outcome.unchanged, 0);
    assert_eq!(outcome.failed, 0);
}

/// The `refresh.rs` half of requirement A's failure case — see
/// `given_the_run_lookup_fails_when_executed_then_the_walk_still_completes_at_the_default_concurrency`
/// in `index.rs` for the full reasoning. `execute` now opens with a `get` to
/// read the run's stored concurrency (Task 9); a transient failure of that
/// read must not sink an otherwise-successful walk.
#[tokio::test]
async fn given_the_run_lookup_fails_when_executed_then_the_walk_still_completes_at_the_default_concurrency(
) {
    let repo = FakeCatalogRepository::new();
    repo.seed(a_cataloged_file_with_hash(
        "/library/a.txt",
        4096,
        Some(now()),
        "old-hash",
    ));
    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/library/a.txt", "a.txt", "unused")
        .with_stat("/library/a.txt", 8192, Some(now()))
        .build();
    let handler = refresh_handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FailingCatalogRunRepository::GetFails,
    );

    let outcome = handler
        .execute(Uuid::new_v4())
        .await
        .expect("a failed concurrency lookup must not fail the walk");

    assert_eq!(outcome.refreshed, 1);
    assert_eq!(outcome.marked_missing, 0);
    assert_eq!(outcome.unchanged, 0);
    assert_eq!(outcome.failed, 0);
}

#[tokio::test]
async fn given_run_cannot_be_started_when_start_then_the_error_propagates() {
    // FR-FC-27: the opposite ruling from the finish/fail case above. A caller
    // must never receive a run id it can never query, so unlike `finish` and
    // `fail`, a `start` recording failure must not be swallowed — it has to
    // reach the caller as an `Err`, not a run id.
    let fs = FakeFilesystem::builder().build();
    let handler = refresh_handler(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        fs,
        fixed_clock(now()),
        FailingCatalogRunRepository::StartFails,
    );

    let result = handler.start(RunPriority::Normal, TOKEN).await;

    assert!(
        result.is_err(),
        "a run-record open failure must propagate, not hand back a run id"
    );
}

#[tokio::test]
async fn given_a_completed_refresh_when_execute_then_the_final_progress_is_flushed_and_the_cell_closed(
) {
    // FR-FC-28: a refresh's discovery is `list_all` rather than a filesystem
    // walk, but it publishes progress exactly as an index does.
    let repo = FakeCatalogRepository::new();
    repo.seed(a_cataloged_file("/library/a.txt", 8192, Some(now())));
    // Absent on disk: it is still an entry the run got through, so it counts
    // toward `processed` like any other.
    repo.seed(a_cataloged_file("/library/b.txt", 4096, Some(now())));
    let fs = FakeFilesystem::builder()
        .with_stat("/library/a.txt", 8192, Some(now()))
        .build();
    let runs = FakeCatalogRunRepository::new();
    let registry = RunRegistry::new();
    let handler = RefreshHandler::new(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        TEST_CONCURRENCY,
        TEST_LOW_PRIORITY_CONCURRENCY,
        runs.clone(),
        registry.clone(),
    );
    let run_id = Uuid::new_v4();
    runs.start(
        run_id,
        RunKind::Refresh,
        None,
        now(),
        TEST_CONCURRENCY,
        None,
    )
    .await
    .unwrap();

    handler.execute(run_id).await.expect("execute");

    let recorded = runs.get_recorded(run_id).expect("recorded run");
    assert_eq!(
        recorded.phase, None,
        "a terminal run has no phase, but keeps its tally"
    );
    assert_eq!(recorded.total, Some(2));
    assert_eq!(recorded.processed, Some(2));
    assert!(
        registry.get(run_id).is_none(),
        "a terminated run must not leave its cell behind"
    );
}

#[tokio::test]
async fn given_a_progress_flush_that_fails_when_execute_then_the_refresh_still_completes() {
    // A flush is best-effort: the cell is authoritative while the run runs,
    // so a failed write must not fail the run.
    let repo = FakeCatalogRepository::new();
    repo.seed(a_cataloged_file("/library/a.txt", 8192, Some(now())));
    let fs = FakeFilesystem::builder()
        .with_stat("/library/a.txt", 8192, Some(now()))
        .build();
    let runs = FakeCatalogRunRepository::with_failing_progress();
    let handler = refresh_handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        runs.clone(),
    );
    let run_id = Uuid::new_v4();
    runs.start(
        run_id,
        RunKind::Refresh,
        None,
        now(),
        TEST_CONCURRENCY,
        None,
    )
    .await
    .unwrap();

    handler.execute(run_id).await.expect("execute");

    assert!(runs.progress_calls() >= 2);
    let recorded = runs.get_recorded(run_id).expect("recorded run");
    assert_eq!(
        recorded.status,
        RunStatus::Complete,
        "a failed flush must not fail the run"
    );
}

/// How many cataloged paths the pause walk below is given. Comfortably more
/// than [`TEST_CONCURRENCY`], so that even if every path already in flight
/// when the signal is raised passes its check, paths are still guaranteed to
/// be left unprocessed.
const HALT_WALK_PATHS: usize = 10;

#[tokio::test]
async fn given_a_refresh_walk_in_flight_when_paused_then_it_stops_with_paths_unprocessed() {
    // A refresh is the same shape as an index — a discovery, then a per-entry
    // loop — so it honours the signal in the same place. The interrupt fires
    // inside the first path's `stat`, which is the first thing `refresh_one`
    // does, so the walk is genuinely under way when the real control handler
    // pauses it.
    let repo = FakeCatalogRepository::new();
    for n in 0..HALT_WALK_PATHS {
        repo.seed(a_cataloged_file(&format!("/library/{n}.txt"), 1, None));
    }
    let runs = FakeCatalogRunRepository::new();
    let registry = RunRegistry::new();
    let run_id = Uuid::new_v4();
    runs.start(
        run_id,
        RunKind::Refresh,
        None,
        now(),
        TEST_CONCURRENCY,
        None,
    )
    .await
    .unwrap();
    let control = Arc::new(RunControlHandler::new(
        FakeAuth::Allowing,
        runs.clone(),
        fixed_clock(now()),
        registry.clone(),
        TEST_CONCURRENCY,
        TEST_LOW_PRIORITY_CONCURRENCY,
    ));
    let pause_mid_walk = interrupt(move || {
        let control = Arc::clone(&control);
        async move { control.pause(run_id, TOKEN).await.expect("pause") }
    });
    let handler = RefreshHandler::new(
        FakeAuth::Allowing,
        repo.clone(),
        InterruptingFilesystem::new(
            FakeFilesystem::builder().build(),
            Seam::FirstStat,
            pause_mid_walk,
        ),
        fixed_clock(now()),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        TEST_CONCURRENCY,
        TEST_LOW_PRIORITY_CONCURRENCY,
        runs.clone(),
        registry.clone(),
    );

    let outcome = handler.execute(run_id).await.expect("execute");

    let tallied = outcome.refreshed + outcome.marked_missing + outcome.unchanged + outcome.failed;
    assert!(
        tallied > 0,
        "the walk must have got started before it was paused"
    );
    assert!(
        tallied < HALT_WALK_PATHS,
        "paths must have been left unprocessed, got {tallied} of {HALT_WALK_PATHS}"
    );
    let recorded = runs.get_recorded(run_id).expect("recorded run");
    assert_eq!(recorded.status, RunStatus::Paused);
    assert!(recorded.paused_at.is_some());
    assert!(recorded.finished_at.is_none());
    assert_eq!(
        recorded.processed,
        Some(tallied),
        "the tally survives the pause, and counts only paths actually processed"
    );
    assert_eq!(recorded.total, Some(HALT_WALK_PATHS));
    assert_eq!(
        recorded.phase,
        Some(RunPhase::Processing),
        "pause is not terminal, so its phase says where the run stopped"
    );
    assert!(
        recorded.counts.is_none(),
        "a paused run writes no tally — a resume supersedes it"
    );
    assert!(
        registry.get(run_id).is_none(),
        "a run that stopped must not leave its cell behind"
    );
}

#[tokio::test]
async fn given_a_run_resumed_before_a_refreshs_pause_lands_when_it_lands_then_the_pause_is_refused()
{
    // The index walk's race, seen at the identical site in a refresh — see
    // `given_a_run_resumed_before_a_walks_pause_lands_…` in the index tests
    // for what goes wrong when the row is `running` again by the time
    // `record_halt` gets there.
    let repo = FakeCatalogRepository::new();
    for n in 0..HALT_WALK_PATHS {
        repo.seed(a_cataloged_file(&format!("/library/{n}.txt"), 1, None));
    }
    let runs = FakeCatalogRunRepository::new();
    let registry = RunRegistry::new();
    let run_id = Uuid::new_v4();
    runs.start(
        run_id,
        RunKind::Refresh,
        None,
        now(),
        TEST_CONCURRENCY,
        None,
    )
    .await
    .unwrap();
    let control = Arc::new(RunControlHandler::new(
        FakeAuth::Allowing,
        runs.clone(),
        fixed_clock(now()),
        registry.clone(),
        TEST_CONCURRENCY,
        TEST_LOW_PRIORITY_CONCURRENCY,
    ));
    let pause_mid_walk = interrupt(move || {
        let control = Arc::clone(&control);
        async move { control.pause(run_id, TOKEN).await.expect("pause") }
    });
    let handler = RefreshHandler::new(
        FakeAuth::Allowing,
        repo.clone(),
        InterruptingFilesystem::new(
            FakeFilesystem::builder().build(),
            Seam::FirstStat,
            pause_mid_walk,
        ),
        fixed_clock(now()),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        TEST_CONCURRENCY,
        TEST_LOW_PRIORITY_CONCURRENCY,
        runs.clone(),
        registry.clone(),
    );
    // Someone else pauses and resumes the run in the gap, after the walk has
    // closed its cell and before its own pause is evaluated.
    runs.pause_and_resume_before_next_halt(run_id);

    handler.execute(run_id).await.expect("execute");

    let recorded = runs.get_recorded(run_id).expect("recorded run");
    assert_eq!(
        recorded.status,
        RunStatus::Running,
        "the resumed segment is walking — the old walk's pause must not have applied"
    );
    assert!(
        recorded.paused_at.is_none(),
        "and it must not have stamped a pause time on a run that is running"
    );
}

#[tokio::test]
async fn given_a_run_resumed_before_a_refreshs_cancel_lands_when_it_lands_then_it_is_refused() {
    // The index walk's terminal-write race, seen at the identical site in a
    // refresh — see `given_a_run_resumed_before_a_walks_cancel_lands_…` in the
    // index tests for why a late cancel is the worse of the two halt verbs.
    let repo = FakeCatalogRepository::new();
    for n in 0..HALT_WALK_PATHS {
        repo.seed(a_cataloged_file(&format!("/library/{n}.txt"), 1, None));
    }
    let runs = FakeCatalogRunRepository::new();
    let registry = RunRegistry::new();
    let run_id = Uuid::new_v4();
    runs.start(
        run_id,
        RunKind::Refresh,
        None,
        now(),
        TEST_CONCURRENCY,
        None,
    )
    .await
    .unwrap();
    let control = Arc::new(RunControlHandler::new(
        FakeAuth::Allowing,
        runs.clone(),
        fixed_clock(now()),
        registry.clone(),
        TEST_CONCURRENCY,
        TEST_LOW_PRIORITY_CONCURRENCY,
    ));
    let cancel_mid_walk = interrupt(move || {
        let control = Arc::clone(&control);
        async move { control.cancel(run_id, TOKEN).await.expect("cancel") }
    });
    let handler = RefreshHandler::new(
        FakeAuth::Allowing,
        repo.clone(),
        InterruptingFilesystem::new(
            FakeFilesystem::builder().build(),
            Seam::FirstStat,
            cancel_mid_walk,
        ),
        fixed_clock(now()),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        TEST_CONCURRENCY,
        TEST_LOW_PRIORITY_CONCURRENCY,
        runs.clone(),
        registry.clone(),
    );
    // Someone else pauses and resumes the run in the gap, after the walk has
    // closed its cell and before its own cancel is evaluated.
    runs.pause_and_resume_before_next_halt(run_id);

    handler.execute(run_id).await.expect("execute");

    let recorded = runs.get_recorded(run_id).expect("recorded run");
    assert_eq!(
        recorded.status,
        RunStatus::Running,
        "the resumed segment is walking — the old walk's cancel must not have applied"
    );
    assert!(
        recorded.finished_at.is_none(),
        "and it must not have stamped a finish time on a run that is running"
    );
    assert!(
        recorded.counts.is_none(),
        "nor written the stopped segment's tally over a run that is still working"
    );
}

#[tokio::test]
async fn given_a_refresh_cancelled_before_its_walk_began_when_executed_then_nothing_is_walked() {
    // The window between `start` and the spawned `execute` opening its cell:
    // a cancel there writes the row itself. The walk used to go ahead anyway
    // and close the run `complete` over the cancel.
    let runs = FakeCatalogRunRepository::new();
    let repo = FakeCatalogRepository::new();
    repo.seed(a_cataloged_file_with_hash(
        "/library/a.txt",
        4096,
        Some(now()),
        "old-hash",
    ));
    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/library/a.txt", "a.txt", "unused")
        .with_stat("/library/a.txt", 8192, Some(now()))
        .build();
    let handler = refresh_handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        runs.clone(),
    );
    let control = RunControlHandler::new(
        FakeAuth::Allowing,
        runs.clone(),
        fixed_clock(now()),
        RunRegistry::new(),
        TEST_CONCURRENCY,
        TEST_LOW_PRIORITY_CONCURRENCY,
    );

    let started = handler
        .start(RunPriority::Normal, TOKEN)
        .await
        .expect("start");
    control.cancel(started.run_id, TOKEN).await.expect("cancel");
    let outcome = handler.execute(started.run_id).await.expect("execute");

    assert_eq!(
        outcome.refreshed, 0,
        "a cancelled refresh walked the catalog"
    );
    let recorded = runs.get_recorded(started.run_id).expect("run recorded");
    assert_eq!(recorded.status, RunStatus::Cancelled);
}
