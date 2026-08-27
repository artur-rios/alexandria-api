use std::sync::Arc;

use uuid::Uuid;

use alexandria_core::auth::AuthService;
use alexandria_core::catalog::audio_tags::{AudioMetadataReader, AudioTags};
use alexandria_core::catalog::classify::classify_by_extension;
use alexandria_core::catalog::clock::{Clock, FixedClock};
use alexandria_core::catalog::comic_tags::{ComicMetadataReader, ComicTags};
use alexandria_core::catalog::commands::index::{IndexHandler, IndexRequest, IndexStarted};
use alexandria_core::catalog::commands::run_control::RunControlHandler;
use alexandria_core::catalog::document_tags::{DocumentMetadataReader, DocumentTags};
use alexandria_core::catalog::fs::Filesystem;
use alexandria_core::catalog::image_tags::{ImageMetadataReader, ImageTags};
use alexandria_core::catalog::model::{FileType, FormatKind, SubtypeMetadata};
use alexandria_core::catalog::repos::CatalogRepository;
use alexandria_core::catalog::run_registry::{RunPhase, RunRegistry};
use alexandria_core::catalog::runs::{
    CatalogRunRepository, RunCounts, RunKind, RunPriority, RunStatus,
};
use alexandria_core::catalog::video_tags::{VideoDuration, VideoMetadataReader, VideoTags};
use alexandria_core::errors::DomainError;

use crate::common::{
    existing_file, fixed_clock, interrupt, now, ConcurrencyTrackingAudioMetadataReader,
    FailingCatalogRunRepository, FailingListFilesystem, FakeAudioMetadataReader, FakeAuth,
    FakeCatalogRepository, FakeCatalogRunRepository, FakeComicMetadataReader,
    FakeDocumentMetadataReader, FakeFilesystem, FakeImageMetadataReader, FakeVideoMetadataReader,
    Interrupt, InterruptingAudioMetadataReader, InterruptingFilesystem, Seam, SteppingClock,
};

const ROOT: &str = "/library";
const TOKEN: &str = "bearer-token";

/// The concurrency these unit tests build handlers with. Deliberately > 1:
/// the outcome tallies must not depend on how many entries are in flight, so
/// exercising the concurrent path is what keeps that true. Tests that assert
/// on a *single* entry are unaffected either way.
const TEST_CONCURRENCY: u32 = 4;

/// The low-priority width these unit tests build handlers with. Deliberately
/// distinct from [`TEST_CONCURRENCY`], so a test that observes which one was
/// actually used cannot pass by accident.
const TEST_LOW_PRIORITY_CONCURRENCY: u32 = 1;

#[allow(clippy::too_many_arguments)]
fn handler<A, R, F, C, M, N, O, P, Q, RR>(
    auth: A,
    repo: R,
    fs: F,
    clock: C,
    audio_tags: M,
    image_tags: N,
    document_tags: O,
    video_tags: P,
    comic_tags: Q,
    runs: RR,
) -> IndexHandler<A, R, F, C, M, N, O, P, Q, RR>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
    C: Clock,
    M: AudioMetadataReader,
    N: ImageMetadataReader,
    O: DocumentMetadataReader,
    P: VideoMetadataReader,
    Q: ComicMetadataReader,
    RR: CatalogRunRepository,
{
    // Empty library root: unconstrained indexing, which is what every test
    // predating FR-FC-26 assumes.
    handler_with_library_root(
        auth,
        repo,
        fs,
        clock,
        audio_tags,
        image_tags,
        document_tags,
        video_tags,
        comic_tags,
        String::new(),
        runs,
    )
}

/// Same as [`handler`], but with an explicit `filesystem.root` bound
/// (FR-FC-26).
#[allow(clippy::too_many_arguments)]
fn handler_with_library_root<A, R, F, C, M, N, O, P, Q, RR>(
    auth: A,
    repo: R,
    fs: F,
    clock: C,
    audio_tags: M,
    image_tags: N,
    document_tags: O,
    video_tags: P,
    comic_tags: Q,
    library_root: String,
    runs: RR,
) -> IndexHandler<A, R, F, C, M, N, O, P, Q, RR>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
    C: Clock,
    M: AudioMetadataReader,
    N: ImageMetadataReader,
    O: DocumentMetadataReader,
    P: VideoMetadataReader,
    Q: ComicMetadataReader,
    RR: CatalogRunRepository,
{
    IndexHandler::new(
        auth,
        repo,
        fs,
        clock,
        audio_tags,
        image_tags,
        document_tags,
        video_tags,
        comic_tags,
        TEST_CONCURRENCY,
        TEST_LOW_PRIORITY_CONCURRENCY,
        library_root,
        runs,
        // Progress goes somewhere no test reads. The tests that do read it
        // build their handler with [`handler_with_registry`].
        RunRegistry::new(),
    )
}

/// Same as [`handler`], but sharing a registry the test can read live
/// progress out of (FR-FC-28).
#[allow(clippy::too_many_arguments)]
fn handler_with_registry<A, R, F, C, M, N, O, P, Q, RR>(
    auth: A,
    repo: R,
    fs: F,
    clock: C,
    audio_tags: M,
    image_tags: N,
    document_tags: O,
    video_tags: P,
    comic_tags: Q,
    runs: RR,
    registry: RunRegistry,
) -> IndexHandler<A, R, F, C, M, N, O, P, Q, RR>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
    C: Clock,
    M: AudioMetadataReader,
    N: ImageMetadataReader,
    O: DocumentMetadataReader,
    P: VideoMetadataReader,
    Q: ComicMetadataReader,
    RR: CatalogRunRepository,
{
    IndexHandler::new(
        auth,
        repo,
        fs,
        clock,
        audio_tags,
        image_tags,
        document_tags,
        video_tags,
        comic_tags,
        TEST_CONCURRENCY,
        TEST_LOW_PRIORITY_CONCURRENCY,
        String::new(),
        runs,
        registry,
    )
}

#[test]
fn given_supported_extensions_when_classify_then_returns_correct_type() {
    assert_eq!(classify_by_extension("song.mp3"), Some(FileType::Audio));
    assert_eq!(classify_by_extension("clip.mkv"), Some(FileType::Video));
    assert_eq!(classify_by_extension("page.html"), Some(FileType::Html));
    assert_eq!(classify_by_extension("notes.md"), Some(FileType::Text));
    assert_eq!(classify_by_extension("book.epub"), Some(FileType::Document));
    assert_eq!(classify_by_extension("comic.cbz"), Some(FileType::Comic));
    assert_eq!(classify_by_extension("pic.png"), Some(FileType::Image));
}

#[tokio::test]
async fn given_valid_root_and_authenticated_when_start_then_returns_run_id() {
    let fs = FakeFilesystem::builder().with_root(ROOT).build();
    let handler = handler(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let started = handler
        .start(
            IndexRequest {
                root: ROOT.to_string(),
                priority: RunPriority::Normal,
            },
            TOKEN,
        )
        .await
        .expect("start");

    assert_ne!(started.run_id, Uuid::nil());
}

#[tokio::test]
async fn given_missing_root_when_start_then_invalid_input() {
    let fs = FakeFilesystem::builder().build();
    let runs = FakeCatalogRunRepository::new();
    let handler = handler(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        runs.clone(),
    );

    let result = handler
        .start(
            IndexRequest {
                root: "/nope".to_string(),
                priority: RunPriority::Normal,
            },
            TOKEN,
        )
        .await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    // FR-FC-27: an invalid root is rejected before the run record opens, so
    // it never leaves a stray record behind.
    assert_eq!(
        runs.count(),
        0,
        "a rejected start must not open a run record"
    );
}

#[tokio::test]
async fn given_unauthenticated_when_start_then_unauthorized() {
    let fs = FakeFilesystem::builder().with_root(ROOT).build();
    let handler = handler(
        FakeAuth::Denying,
        FakeCatalogRepository::new(),
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let result = handler
        .start(
            IndexRequest {
                root: ROOT.to_string(),
                priority: RunPriority::Normal,
            },
            "",
        )
        .await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

#[tokio::test]
async fn given_already_cataloged_path_when_execute_then_already_cataloged_no_duplicate() {
    let existing_path = "/library/a.mp3";
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, existing_path, "a.mp3", "h-a")
        .with_file(ROOT, "/library/b.mp3", "b.mp3", "h-b")
        .build();

    // Clone the repo so we can inspect shared state after the handler owns its
    // own move of the original clone.
    let repo = FakeCatalogRepository::with_existing(existing_file(existing_path, FileType::Audio));
    let repo_handle = repo.clone();
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.scanned, 2);
    assert_eq!(
        outcome.indexed, 1,
        "the already-cataloged path must be skipped"
    );
    assert_eq!(
        outcome.already_cataloged, 1,
        "the existing path is already in the catalog, not an unsupported extension"
    );
    assert_eq!(outcome.skipped, 0);
    assert_eq!(
        repo_handle.count(),
        2,
        "exactly the existing + one new record"
    );
    assert!(repo_handle.has_path(existing_path));
    assert!(repo_handle.has_path("/library/b.mp3"));
}

#[tokio::test]
async fn given_supported_files_when_execute_then_indexed_with_no_hash_and_indexedat() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.mp3", "a.mp3", "h-a")
        .with_file(ROOT, "/library/b.md", "b.md", "h-b")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.scanned, 2);
    assert_eq!(outcome.indexed, 2);
    assert_eq!(outcome.skipped, 0);

    let a = repo_handle.file_for("/library/a.mp3").expect("a indexed");
    let b = repo_handle.file_for("/library/b.md").expect("b indexed");
    assert_eq!(
        a.content_hash, None,
        "indexing never hashes bytes (FR-FC-09)"
    );
    assert_eq!(b.content_hash, None);
    assert_eq!(a.indexed_at, now());
    assert_eq!(b.indexed_at, now());
    assert_eq!(a.file_type, FileType::Audio);
    assert_eq!(b.file_type, FileType::Text);
}

#[tokio::test]
async fn given_a_file_on_disk_when_indexed_then_its_size_and_mtime_are_recorded() {
    let fs = FakeFilesystem::builder()
        .with_root(ROOT)
        .with_file(ROOT, "/library/song.txt", "song.txt", "hash-1")
        .with_stat("/library/song.txt", 10, Some(now()))
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let IndexStarted { run_id } = handler
        .start(
            IndexRequest {
                root: ROOT.into(),
                priority: RunPriority::Normal,
            },
            TOKEN,
        )
        .await
        .unwrap();
    handler.execute(ROOT, run_id).await.unwrap();

    let file = repo_handle
        .find_by_path("/library/song.txt")
        .await
        .unwrap()
        .expect("file should be cataloged");

    assert_eq!(file.size_bytes, Some(10));
    assert_eq!(file.mtime, Some(now()), "mtime is captured from the walk");
}

#[tokio::test]
async fn given_a_file_when_indexed_then_no_content_hash_is_computed() {
    let fs = FakeFilesystem::builder()
        .with_root(ROOT)
        .with_file(ROOT, "/library/song.txt", "song.txt", "hash-1")
        .build();
    let fs_handle = fs.clone();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let IndexStarted { run_id } = handler
        .start(
            IndexRequest {
                root: ROOT.into(),
                priority: RunPriority::Normal,
            },
            TOKEN,
        )
        .await
        .unwrap();
    handler.execute(ROOT, run_id).await.unwrap();

    let file = repo_handle
        .find_by_path("/library/song.txt")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        file.content_hash, None,
        "indexing must not read file bytes; the hash is computed on demand"
    );
    assert_eq!(
        fs_handle.hash_calls(),
        0,
        "the filesystem's hash port is never reached"
    );
}

// ---------------- Bounded concurrency (FR-FC-08) ----------------

/// The walk processes several files at a time, so the order entries finish in
/// is unspecified — but the tallies are not. Running the same library at every
/// concurrency from sequential to wider-than-the-library must produce
/// identical counts and identical catalog contents.
///
/// `c.mp3` is seeded unreadable (its bytes cannot be hashed), but that no
/// longer matters to indexing at all (Task 3 / FR-FC-09): `index_entry`
/// never calls the hash port, so an unreadable-but-listable file is indexed
/// exactly like a readable one. `failing_for` on the repository — not an
/// unreadable filesystem entry — is what still produces a per-file `failed`
/// after this task; see
/// `given_failing_repository_write_when_execute_then_run_continues_and_counts_failure`.
#[tokio::test]
async fn given_any_concurrency_when_execute_then_same_counts_and_same_catalog() {
    // 1 (sequential), 3 (narrower than the library), 4 (exactly), and 16
    // (wider than the library — the buffer never fills).
    for concurrency in [1u32, 3, 4, 16] {
        let fs = FakeFilesystem::builder()
            .with_file(ROOT, "/library/a.mp3", "a.mp3", "h-a")
            .with_file(ROOT, "/library/b.md", "b.md", "h-b")
            .with_unreadable_file(ROOT, "/library/c.mp3", "c.mp3")
            .with_file(ROOT, "/library/d.zip", "d.zip", "h-d")
            .build();
        let repo = FakeCatalogRepository::new();
        let repo_handle = repo.clone();
        let handler = IndexHandler::new(
            FakeAuth::Allowing,
            repo,
            fs,
            fixed_clock(now()),
            FakeAudioMetadataReader::new(),
            FakeImageMetadataReader::new(),
            FakeDocumentMetadataReader::new(),
            FakeVideoMetadataReader::new(),
            FakeComicMetadataReader::new(),
            concurrency,
            TEST_LOW_PRIORITY_CONCURRENCY,
            String::new(),
            FakeCatalogRunRepository::new(),
            RunRegistry::new(),
        );

        let outcome = handler
            .execute(ROOT, Uuid::new_v4())
            .await
            .expect("execute");

        assert_eq!(outcome.scanned, 4, "concurrency {concurrency}");
        assert_eq!(
            outcome.indexed, 3,
            "the unreadable mp3 is indexed too — its bytes are never read (concurrency {concurrency})"
        );
        assert_eq!(
            outcome.skipped, 1,
            "the .zip is unsupported (concurrency {concurrency})"
        );
        assert_eq!(outcome.failed, 0, "concurrency {concurrency}");
        assert!(repo_handle.has_path("/library/a.mp3"));
        assert!(repo_handle.has_path("/library/b.md"));
        assert!(repo_handle.has_path("/library/c.mp3"));
        assert!(!repo_handle.has_path("/library/d.zip"));
    }
}

/// Zero is clamped to sequential. A stream buffered zero deep yields nothing
/// and the run would hang forever, so this asserts the run *completes* — the
/// counts are incidental; the point is that it returns at all.
#[tokio::test]
async fn given_zero_concurrency_when_execute_then_runs_sequentially_rather_than_hanging() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.mp3", "a.mp3", "h-a")
        .with_file(ROOT, "/library/b.mp3", "b.mp3", "h-b")
        .build();
    let handler = IndexHandler::new(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        0,
        TEST_LOW_PRIORITY_CONCURRENCY,
        String::new(),
        FakeCatalogRunRepository::new(),
        RunRegistry::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 2);
}

/// Requirement D: zero is exactly as meaningless for the `Low` width as it is
/// for the `Normal` one, and gets the same clamp. A run started at `Low`
/// against a misconfigured `indexing.low_priority_concurrency = 0` must still
/// make progress rather than hang on a stream buffered zero deep.
#[tokio::test]
async fn given_zero_low_priority_concurrency_when_a_low_priority_run_executes_then_runs_sequentially_rather_than_hanging(
) {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.mp3", "a.mp3", "h-a")
        .with_file(ROOT, "/library/b.mp3", "b.mp3", "h-b")
        .build();
    let runs = FakeCatalogRunRepository::new();
    let handler = IndexHandler::new(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        TEST_CONCURRENCY,
        0,
        String::new(),
        runs,
        RunRegistry::new(),
    );

    let IndexStarted { run_id } = handler
        .start(
            IndexRequest {
                root: ROOT.into(),
                priority: RunPriority::Low,
            },
            TOKEN,
        )
        .await
        .unwrap();
    let outcome = handler.execute(ROOT, run_id).await.expect("execute");

    assert_eq!(outcome.indexed, 2);
}

#[tokio::test]
async fn given_unsupported_extension_when_execute_then_skipped() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/readme", "readme", "h-1")
        .with_file(ROOT, "/library/archive.zip", "archive.zip", "h-2")
        .build();
    let repo = FakeCatalogRepository::new();
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.scanned, 2);
    assert_eq!(outcome.indexed, 0);
    assert_eq!(outcome.skipped, 2);
}

/// The payoff of Task 3 (FR-FC-09): a file whose bytes cannot be read is no
/// obstacle to indexing at all, because `index_entry` never calls the hash
/// port. Before this task, `b.mp3` here would have failed to hash and been
/// counted in `failed`; now it is indexed exactly like its readable
/// neighbours.
#[tokio::test]
async fn given_unreadable_file_when_execute_then_it_is_indexed_anyway() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.mp3", "a.mp3", "h-a")
        .with_unreadable_file(ROOT, "/library/b.mp3", "b.mp3")
        .with_file(ROOT, "/library/c.mp3", "c.mp3", "h-c")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("an unreadable file must not fail the whole run");

    assert_eq!(outcome.scanned, 3);
    assert_eq!(
        outcome.indexed, 3,
        "every file is indexed, including the unreadable one"
    );
    assert_eq!(
        outcome.failed, 0,
        "indexing never reads bytes, so nothing fails here"
    );
    assert_eq!(outcome.skipped, 0);
    assert!(repo_handle.has_path("/library/a.mp3"));
    assert!(repo_handle.has_path("/library/c.mp3"));
    assert!(
        repo_handle.has_path("/library/b.mp3"),
        "the unreadable file is cataloged too, with content_hash left None"
    );
    assert_eq!(
        repo_handle
            .file_for("/library/b.mp3")
            .expect("b indexed")
            .content_hash,
        None
    );
}

#[tokio::test]
async fn given_failing_repository_write_when_execute_then_run_continues_and_counts_failure() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.mp3", "a.mp3", "h-a")
        .with_file(ROOT, "/library/b.mp3", "b.mp3", "h-b")
        .build();
    let repo = FakeCatalogRepository::new().failing_for("/library/a.mp3");
    let repo_handle = repo.clone();
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("a per-file repository error must not fail the whole run");

    assert_eq!(outcome.indexed, 1);
    assert_eq!(outcome.failed, 1);
    assert!(repo_handle.has_path("/library/b.mp3"), "b still indexed");
}

#[tokio::test]
async fn given_bearer_auth_when_authenticated_then_principal_owner() {
    let principal = alexandria_core::auth::BearerAuthService
        .authenticate("some-bearer")
        .await
        .expect("auth");
    assert_eq!(principal.user_id, "owner");
}

#[tokio::test]
async fn given_bearer_auth_when_empty_token_then_unauthorized() {
    let result = alexandria_core::auth::BearerAuthService
        .authenticate("  ")
        .await;
    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

#[test]
fn given_fixed_clock_when_now_then_returns_seeded_time() {
    let clock = fixed_clock(now());
    assert_eq!(clock.now(), now());
}

#[tokio::test]
async fn given_tagged_audio_file_when_execute_then_subtype_metadata_written() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.mp3", "a.mp3", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let audio_tags = FakeAudioMetadataReader::new();
    audio_tags.seed(
        "/library/a.mp3",
        AudioTags {
            title: Some("Song".to_string()),
            artist: Some("Band".to_string()),
            album: Some("LP".to_string()),
            year: Some(2001),
            genre: Some("Rock".to_string()),
            track: Some(4),
            album_artist: Some("Various Artists".to_string()),
        },
    );
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        audio_tags,
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.mp3").expect("indexed");
    let metadata = repo_handle
        .metadata_for(a.uuid)
        .expect("metadata written from extracted tags");
    assert_eq!(
        metadata,
        SubtypeMetadata::Audio {
            title: Some("Song".to_string()),
            artist: Some("Band".to_string()),
            album: Some("LP".to_string()),
            year: Some(2001),
            genre: Some("Rock".to_string()),
            track: Some(4),
            album_artist: Some("Various Artists".to_string()),
        }
    );
}

#[tokio::test]
async fn given_untagged_audio_file_when_execute_then_subtype_metadata_stays_empty() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.mp3", "a.mp3", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    // No tags seeded — FakeAudioMetadataReader::read returns None for any
    // unseeded path.
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.mp3").expect("indexed");
    assert!(
        repo_handle.metadata_for(a.uuid).is_none(),
        "no tags found means no update_metadata call"
    );
}

#[tokio::test]
async fn given_non_audio_file_when_execute_then_reader_never_consulted() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/notes.md", "notes.md", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let audio_tags = FakeAudioMetadataReader::new();
    let audio_tags_handle = audio_tags.clone();
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        audio_tags,
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let notes = repo_handle.file_for("/library/notes.md").expect("indexed");
    assert_eq!(notes.file_type, FileType::Text);
    assert_eq!(
        audio_tags_handle.call_count(),
        0,
        "the reader must not be consulted at all for a non-audio file"
    );
    assert!(
        repo_handle.metadata_for(notes.uuid).is_none(),
        "Text has no SubtypeMetadata variant; extraction must not run for it"
    );
}

#[tokio::test]
async fn given_tagged_image_file_when_execute_then_dimensions_and_title_written() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.jpg", "a.jpg", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let image_tags = FakeImageMetadataReader::new();
    image_tags.seed(
        "/library/a.jpg",
        ImageTags {
            width: Some(800),
            height: Some(600),
            title: Some("A Photo".to_string()),
        },
    );
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        image_tags,
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.jpg").expect("indexed");
    assert_eq!(repo_handle.dimensions_for(a.uuid), Some((800, 600)));
    let metadata = repo_handle
        .metadata_for(a.uuid)
        .expect("title written from extracted tags");
    assert_eq!(
        metadata,
        alexandria_core::catalog::model::SubtypeMetadata::Image {
            title: Some("A Photo".to_string()),
            caption: None,
        }
    );
}

#[tokio::test]
async fn given_image_with_dimensions_but_no_title_when_execute_then_only_dimensions_written() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.jpg", "a.jpg", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let image_tags = FakeImageMetadataReader::new();
    image_tags.seed(
        "/library/a.jpg",
        ImageTags {
            width: Some(800),
            height: Some(600),
            title: None,
        },
    );
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        image_tags,
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.jpg").expect("indexed");
    assert_eq!(repo_handle.dimensions_for(a.uuid), Some((800, 600)));
    assert!(
        repo_handle.metadata_for(a.uuid).is_none(),
        "no title extracted means update_metadata is never called"
    );
}

#[tokio::test]
async fn given_image_with_title_but_no_dimensions_when_execute_then_only_title_written() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.jpg", "a.jpg", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let image_tags = FakeImageMetadataReader::new();
    image_tags.seed(
        "/library/a.jpg",
        ImageTags {
            width: None,
            height: None,
            title: Some("A Photo".to_string()),
        },
    );
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        image_tags,
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.jpg").expect("indexed");
    assert_eq!(
        repo_handle.dimensions_for(a.uuid),
        None,
        "no width/height extracted means set_image_dimensions is never called"
    );
    let metadata = repo_handle
        .metadata_for(a.uuid)
        .expect("title written from extracted tags");
    assert_eq!(
        metadata,
        alexandria_core::catalog::model::SubtypeMetadata::Image {
            title: Some("A Photo".to_string()),
            caption: None,
        }
    );
}

#[tokio::test]
async fn given_image_with_partial_dimensions_when_execute_then_dimensions_write_skipped() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.jpg", "a.jpg", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let image_tags = FakeImageMetadataReader::new();
    image_tags.seed(
        "/library/a.jpg",
        ImageTags {
            width: Some(800),
            height: None,
            title: None,
        },
    );
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        image_tags,
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.jpg").expect("indexed");
    assert_eq!(
        repo_handle.dimensions_for(a.uuid),
        None,
        "only one of width/height present must skip the dimensions write"
    );
    assert!(
        repo_handle.metadata_for(a.uuid).is_none(),
        "no title extracted means update_metadata is never called"
    );
}

#[tokio::test]
async fn given_untagged_image_file_when_execute_then_neither_write_happens() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.jpg", "a.jpg", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.jpg").expect("indexed");
    assert_eq!(repo_handle.dimensions_for(a.uuid), None);
    assert!(repo_handle.metadata_for(a.uuid).is_none());
}

#[tokio::test]
async fn given_non_image_file_when_execute_then_image_reader_never_consulted() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/notes.md", "notes.md", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let audio_tags = FakeAudioMetadataReader::new();
    let image_tags = FakeImageMetadataReader::new();
    let image_tags_handle = image_tags.clone();
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        audio_tags,
        image_tags,
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    assert_eq!(
        image_tags_handle.call_count(),
        0,
        "the image reader must not be consulted at all for a non-image file"
    );
}

#[tokio::test]
async fn given_tagged_pdf_when_execute_then_page_count_and_metadata_written() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.pdf", "a.pdf", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let document_tags = FakeDocumentMetadataReader::new();
    document_tags.seed(
        "/library/a.pdf",
        DocumentTags {
            title: Some("A Book".to_string()),
            author: Some("An Author".to_string()),
            year: None,
            format_kind: Some(FormatKind::Book),
            page_count: Some(42),
        },
    );
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        document_tags,
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.pdf").expect("indexed");
    assert_eq!(repo_handle.document_page_count_for(a.uuid), Some(42));
    let metadata = repo_handle
        .metadata_for(a.uuid)
        .expect("metadata written from extracted tags");
    assert_eq!(
        metadata,
        SubtypeMetadata::Document {
            title: Some("A Book".to_string()),
            author: Some("An Author".to_string()),
            year: None,
            format_kind: Some(FormatKind::Book),
        }
    );
}

#[tokio::test]
async fn given_tagged_epub_when_execute_then_metadata_written_no_page_count() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.epub", "a.epub", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let document_tags = FakeDocumentMetadataReader::new();
    document_tags.seed(
        "/library/a.epub",
        DocumentTags {
            title: Some("An Ebook".to_string()),
            author: Some("An Author".to_string()),
            year: None,
            format_kind: Some(FormatKind::Ebook),
            page_count: None,
        },
    );
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        document_tags,
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.epub").expect("indexed");
    assert_eq!(
        repo_handle.document_page_count_for(a.uuid),
        None,
        "EPUB never sets page_count"
    );
    let metadata = repo_handle
        .metadata_for(a.uuid)
        .expect("metadata written from extracted tags");
    assert_eq!(
        metadata,
        SubtypeMetadata::Document {
            title: Some("An Ebook".to_string()),
            author: Some("An Author".to_string()),
            year: None,
            format_kind: Some(FormatKind::Ebook),
        }
    );
}

#[tokio::test]
async fn given_document_with_page_count_but_no_other_fields_when_execute_then_both_writes_happen() {
    // format_kind is always Some whenever extraction identifies the file
    // as PDF/EPUB at all, so even "no title/author/year" still triggers
    // the metadata write — this test proves that, distinct from the
    // audio/image sibling tests where an all-empty tag set skips the
    // metadata write entirely.
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.pdf", "a.pdf", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let document_tags = FakeDocumentMetadataReader::new();
    document_tags.seed(
        "/library/a.pdf",
        DocumentTags {
            title: None,
            author: None,
            year: None,
            format_kind: Some(FormatKind::Book),
            page_count: Some(10),
        },
    );
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        document_tags,
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.pdf").expect("indexed");
    assert_eq!(repo_handle.document_page_count_for(a.uuid), Some(10));
    let metadata = repo_handle
        .metadata_for(a.uuid)
        .expect("format_kind alone triggers the metadata write");
    assert_eq!(
        metadata,
        SubtypeMetadata::Document {
            title: None,
            author: None,
            year: None,
            format_kind: Some(FormatKind::Book),
        }
    );
}

#[tokio::test]
async fn given_untagged_document_file_when_execute_then_neither_write_happens() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.pdf", "a.pdf", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.pdf").expect("indexed");
    assert_eq!(repo_handle.document_page_count_for(a.uuid), None);
    assert!(repo_handle.metadata_for(a.uuid).is_none());
}

#[tokio::test]
async fn given_non_document_file_when_execute_then_document_reader_never_consulted() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/notes.md", "notes.md", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let audio_tags = FakeAudioMetadataReader::new();
    let image_tags = FakeImageMetadataReader::new();
    let document_tags = FakeDocumentMetadataReader::new();
    let document_tags_handle = document_tags.clone();
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        audio_tags,
        image_tags,
        document_tags,
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    assert_eq!(
        document_tags_handle.call_count(),
        0,
        "the document reader must not be consulted at all for a non-document file"
    );
}

#[tokio::test]
async fn given_tagged_video_when_execute_then_duration_and_metadata_written() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.mp4", "a.mp4", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let video_tags = FakeVideoMetadataReader::new();
    video_tags.seed(
        "/library/a.mp4",
        VideoTags {
            title: Some("A Movie".to_string()),
            year: Some(2020),
            resolution: Some("1920x1080".to_string()),
            duration_seconds: Some(VideoDuration(125.5)),
        },
    );
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        video_tags,
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.mp4").expect("indexed");
    assert_eq!(repo_handle.video_duration_for(a.uuid), Some(125.5));
    let metadata = repo_handle
        .metadata_for(a.uuid)
        .expect("metadata written from extracted tags");
    assert_eq!(
        metadata,
        SubtypeMetadata::Video {
            title: Some("A Movie".to_string()),
            year: Some(2020),
            resolution: Some("1920x1080".to_string()),
            media_kind: None,
        }
    );
}

#[tokio::test]
async fn given_video_with_duration_but_no_other_fields_when_execute_then_only_duration_written() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.mp4", "a.mp4", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let video_tags = FakeVideoMetadataReader::new();
    video_tags.seed(
        "/library/a.mp4",
        VideoTags {
            title: None,
            year: None,
            resolution: None,
            duration_seconds: Some(VideoDuration(60.0)),
        },
    );
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        video_tags,
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.mp4").expect("indexed");
    assert_eq!(repo_handle.video_duration_for(a.uuid), Some(60.0));
    assert!(
        repo_handle.metadata_for(a.uuid).is_none(),
        "no title/year/resolution extracted means update_metadata is never called"
    );
}

#[tokio::test]
async fn given_video_with_resolution_but_no_duration_when_execute_then_only_metadata_written() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.mp4", "a.mp4", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let video_tags = FakeVideoMetadataReader::new();
    video_tags.seed(
        "/library/a.mp4",
        VideoTags {
            title: None,
            year: None,
            resolution: Some("1280x720".to_string()),
            duration_seconds: None,
        },
    );
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        video_tags,
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.mp4").expect("indexed");
    assert_eq!(
        repo_handle.video_duration_for(a.uuid),
        None,
        "no duration extracted means set_video_duration is never called"
    );
    let metadata = repo_handle
        .metadata_for(a.uuid)
        .expect("resolution written from extracted tags");
    assert_eq!(
        metadata,
        SubtypeMetadata::Video {
            title: None,
            year: None,
            resolution: Some("1280x720".to_string()),
            media_kind: None,
        }
    );
}

#[tokio::test]
async fn given_untagged_video_file_when_execute_then_neither_write_happens() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.mp4", "a.mp4", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.mp4").expect("indexed");
    assert_eq!(repo_handle.video_duration_for(a.uuid), None);
    assert!(repo_handle.metadata_for(a.uuid).is_none());
}

#[tokio::test]
async fn given_non_video_file_when_execute_then_video_reader_never_consulted() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/notes.md", "notes.md", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let audio_tags = FakeAudioMetadataReader::new();
    let image_tags = FakeImageMetadataReader::new();
    let document_tags = FakeDocumentMetadataReader::new();
    let video_tags = FakeVideoMetadataReader::new();
    let video_tags_handle = video_tags.clone();
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        audio_tags,
        image_tags,
        document_tags,
        video_tags,
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    assert_eq!(
        video_tags_handle.call_count(),
        0,
        "the video reader must not be consulted at all for a non-video file"
    );
}

#[tokio::test]
async fn given_tagged_comic_when_execute_then_page_count_and_metadata_written() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.cbz", "a.cbz", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let comic_tags = FakeComicMetadataReader::new();
    comic_tags.seed(
        "/library/a.cbz",
        ComicTags {
            title: Some("A Comic".to_string()),
            series: Some("A Series".to_string()),
            issue_number: Some(3),
            page_count: Some(24),
        },
    );
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        comic_tags,
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.cbz").expect("indexed");
    assert_eq!(repo_handle.comic_page_count_for(a.uuid), Some(24));
    let metadata = repo_handle
        .metadata_for(a.uuid)
        .expect("metadata written from extracted tags");
    assert_eq!(
        metadata,
        SubtypeMetadata::Comic {
            title: Some("A Comic".to_string()),
            series: Some("A Series".to_string()),
            issue_number: Some(3),
        }
    );
}

#[tokio::test]
async fn given_comic_with_page_count_but_no_other_fields_when_execute_then_only_page_count_written()
{
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.cbz", "a.cbz", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let comic_tags = FakeComicMetadataReader::new();
    comic_tags.seed(
        "/library/a.cbz",
        ComicTags {
            title: None,
            series: None,
            issue_number: None,
            page_count: Some(10),
        },
    );
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        comic_tags,
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.cbz").expect("indexed");
    assert_eq!(repo_handle.comic_page_count_for(a.uuid), Some(10));
    assert!(
        repo_handle.metadata_for(a.uuid).is_none(),
        "no title/series/issue_number extracted means update_metadata is never called"
    );
}

#[tokio::test]
async fn given_comic_with_issue_number_but_no_page_count_when_execute_then_only_metadata_written() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.cbz", "a.cbz", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let comic_tags = FakeComicMetadataReader::new();
    comic_tags.seed(
        "/library/a.cbz",
        ComicTags {
            title: None,
            series: None,
            issue_number: Some(7),
            page_count: None,
        },
    );
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        comic_tags,
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.cbz").expect("indexed");
    assert_eq!(
        repo_handle.comic_page_count_for(a.uuid),
        None,
        "no page_count extracted means set_comic_page_count is never called"
    );
    let metadata = repo_handle
        .metadata_for(a.uuid)
        .expect("issue_number alone triggers the metadata write");
    assert_eq!(
        metadata,
        SubtypeMetadata::Comic {
            title: None,
            series: None,
            issue_number: Some(7),
        }
    );
}

#[tokio::test]
async fn given_unopenable_comic_file_when_execute_then_neither_write_happens() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.cbz", "a.cbz", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.cbz").expect("indexed");
    assert_eq!(repo_handle.comic_page_count_for(a.uuid), None);
    assert!(repo_handle.metadata_for(a.uuid).is_none());
}

#[tokio::test]
async fn given_non_comic_file_when_execute_then_comic_reader_never_consulted() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/notes.md", "notes.md", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let audio_tags = FakeAudioMetadataReader::new();
    let image_tags = FakeImageMetadataReader::new();
    let document_tags = FakeDocumentMetadataReader::new();
    let video_tags = FakeVideoMetadataReader::new();
    let comic_tags = FakeComicMetadataReader::new();
    let comic_tags_handle = comic_tags.clone();
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        audio_tags,
        image_tags,
        document_tags,
        video_tags,
        comic_tags,
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    assert_eq!(
        comic_tags_handle.call_count(),
        0,
        "the comic reader must not be consulted at all for a non-comic file"
    );
}

// -------- FR-FC-26: the index root must sit inside `filesystem.root` --------
//
// These tests need paths `std::fs::canonicalize` can actually resolve, so they
// use real temp directories rather than the `/library` literal the rest of this
// file uses. The `FakeFilesystem` still stands in for the walk; only the
// containment check touches the real disk.

/// Start an index bounded by `library_root`. `requested` is registered as an
/// existing root on the fake filesystem so the pre-existing "root path does
/// not exist" guard (AF-01) passes and the containment check (AF-06) is what
/// decides the outcome.
async fn start_bounded(
    library_root: &std::path::Path,
    requested: &str,
) -> Result<IndexStarted, DomainError> {
    start_bounded_with_runs(library_root, requested, FakeCatalogRunRepository::new()).await
}

/// Like [`start_bounded`], but takes the run repository fake so a caller can
/// keep a handle on it and assert what it did or did not record.
async fn start_bounded_with_runs(
    library_root: &std::path::Path,
    requested: &str,
    runs: FakeCatalogRunRepository,
) -> Result<IndexStarted, DomainError> {
    let handler = handler_with_library_root(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        FakeFilesystem::builder().with_root(requested).build(),
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        library_root
            .to_str()
            .expect("utf-8 library root")
            .to_string(),
        runs,
    );
    handler
        .start(
            IndexRequest {
                root: requested.to_string(),
                priority: RunPriority::Normal,
            },
            TOKEN,
        )
        .await
}

#[tokio::test]
async fn given_root_inside_configured_library_root_when_start_then_returns_run_id() {
    // Arrange
    let library = tempfile::tempdir().expect("tempdir");
    let inside = library.path().join("music");
    std::fs::create_dir(&inside).expect("create inside");
    let requested = inside.to_str().expect("utf-8").to_string();

    // Act
    let result = start_bounded(library.path(), &requested).await;

    // Assert
    assert_ne!(result.expect("start").run_id, Uuid::nil());
}

#[tokio::test]
async fn given_configured_library_root_itself_when_start_then_returns_run_id() {
    // Arrange
    let library = tempfile::tempdir().expect("tempdir");
    let requested = library.path().to_str().expect("utf-8").to_string();

    // Act
    let result = start_bounded(library.path(), &requested).await;

    // Assert
    assert_ne!(result.expect("start").run_id, Uuid::nil());
}

#[tokio::test]
async fn given_root_outside_configured_library_root_when_start_then_invalid_input() {
    // Arrange
    let parent = tempfile::tempdir().expect("tempdir");
    let library = parent.path().join("library");
    let outside = parent.path().join("secrets");
    std::fs::create_dir(&library).expect("create library");
    std::fs::create_dir(&outside).expect("create outside");
    let requested = outside.to_str().expect("utf-8").to_string();
    let runs = FakeCatalogRunRepository::new();

    // Act
    let result = start_bounded_with_runs(&library, &requested, runs.clone()).await;

    // Assert
    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    // FR-FC-27: rejected before the run record opens — see the equivalent
    // assertion in `given_missing_root_when_start_then_invalid_input`.
    assert_eq!(
        runs.count(),
        0,
        "a rejected start must not open a run record"
    );
}

/// The traversal case: the requested root is spelled as a path *inside* the
/// library that climbs back out with `..`. A string comparison would accept it
/// — the text does start with the library root's text. Canonicalizing first
/// resolves the `..` away and the escape becomes visible.
#[tokio::test]
async fn given_traversal_escaping_library_root_when_start_then_invalid_input() {
    // Arrange
    let parent = tempfile::tempdir().expect("tempdir");
    let library = parent.path().join("library");
    let outside = parent.path().join("secrets");
    std::fs::create_dir(&library).expect("create library");
    std::fs::create_dir(&outside).expect("create outside");
    let requested = library
        .join("..")
        .join("secrets")
        .to_str()
        .expect("utf-8")
        .to_string();

    // Act
    let result = start_bounded(&library, &requested).await;

    // Assert
    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
}

/// The prefix case: `<tmp>/lib-evil` is a sibling of `<tmp>/lib`, not a
/// descendant, yet its string starts with the library root's string. Only a
/// component-wise comparison rejects it.
#[tokio::test]
async fn given_sibling_root_sharing_a_name_prefix_when_start_then_invalid_input() {
    // Arrange
    let parent = tempfile::tempdir().expect("tempdir");
    let library = parent.path().join("lib");
    let sibling = parent.path().join("lib-evil");
    std::fs::create_dir(&library).expect("create library");
    std::fs::create_dir(&sibling).expect("create sibling");
    let requested = sibling.to_str().expect("utf-8").to_string();

    // Act
    let result = start_bounded(&library, &requested).await;

    // Assert
    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
}

/// The backward-compatibility guarantee: an unset `filesystem.root` leaves
/// indexing exactly as unconstrained as it was before FR-FC-26 existed.
#[tokio::test]
async fn given_empty_configured_library_root_when_start_then_any_root_accepted() {
    // Arrange — a path that does not exist on the test host. If the
    // empty-root early return were ever deleted, `check_root_within_library`
    // would try to canonicalize this and fail, so this test would catch that
    // removal instead of passing incidentally on a real tempdir.
    let requested = "/library".to_string();
    let handler = handler(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        FakeFilesystem::builder().with_root(&requested).build(),
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    // Act
    let started = handler
        .start(
            IndexRequest {
                root: requested,
                priority: RunPriority::Normal,
            },
            TOKEN,
        )
        .await;

    // Assert
    assert_ne!(started.expect("start").run_id, Uuid::nil());
}

/// The fail-closed guarantee: a *configured* `filesystem.root` that cannot be
/// canonicalized (e.g. a config typo naming a path that does not exist) must
/// reject the request rather than silently falling back to unconstrained
/// indexing. Without this test, replacing the fallible branch with a silent
/// `Ok(())` would leave every other test in this file passing.
#[tokio::test]
async fn given_unresolvable_configured_library_root_when_start_then_invalid_input() {
    // Arrange
    let requested_dir = tempfile::tempdir().expect("tempdir");
    let requested = requested_dir.path().to_str().expect("utf-8").to_string();
    let handler = handler_with_library_root(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        FakeFilesystem::builder().with_root(&requested).build(),
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        "/nonexistent-library-root".to_string(),
        FakeCatalogRunRepository::new(),
    );

    // Act
    let result = handler
        .start(
            IndexRequest {
                root: requested,
                priority: RunPriority::Normal,
            },
            TOKEN,
        )
        .await;

    // Assert
    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
}

// ---------------- Run record lifecycle (UC-42 / FR-FC-27) ----------------

#[tokio::test]
async fn given_a_started_index_when_started_then_the_run_is_recorded_running() {
    let runs = FakeCatalogRunRepository::new();
    let fs = FakeFilesystem::builder().with_root(ROOT).build();
    let handler = handler(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        runs.clone(),
    );

    let started = handler
        .start(
            IndexRequest {
                root: ROOT.to_string(),
                priority: RunPriority::Normal,
            },
            TOKEN,
        )
        .await
        .expect("start");

    let recorded = runs.get_recorded(started.run_id).expect("run recorded");
    assert_eq!(recorded.kind, RunKind::Index);
    assert_eq!(recorded.status, RunStatus::Running);
    assert_eq!(recorded.root, Some(ROOT.to_string()));
}

/// The write side of run priority (FR-FC-08 / Task 9): a run started at `Low`
/// records the low-priority width, not the normal one, on its own row. This
/// is what makes `CatalogRunRepository::start`'s `concurrency` column
/// something other than always-NULL.
#[tokio::test]
async fn given_a_low_priority_index_when_started_then_the_run_records_the_low_concurrency() {
    let runs = FakeCatalogRunRepository::new();
    let fs = FakeFilesystem::builder().with_root(ROOT).build();
    let handler = handler(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        runs.clone(),
    );

    let IndexStarted { run_id } = handler
        .start(
            IndexRequest {
                root: ROOT.into(),
                priority: RunPriority::Low,
            },
            TOKEN,
        )
        .await
        .unwrap();

    assert_eq!(
        runs.get_recorded(run_id).unwrap().concurrency,
        Some(TEST_LOW_PRIORITY_CONCURRENCY)
    );
}

/// The `Normal` counterpart of the test above — pinned so a regression that
/// swapped the two `match` arms in `IndexHandler::concurrency_for` would be
/// caught by *some* test even if it happened to leave `Low` looking right.
#[tokio::test]
async fn given_a_normal_priority_index_when_started_then_the_run_records_the_normal_concurrency() {
    let runs = FakeCatalogRunRepository::new();
    let fs = FakeFilesystem::builder().with_root(ROOT).build();
    let handler = handler(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        runs.clone(),
    );

    let IndexStarted { run_id } = handler
        .start(
            IndexRequest {
                root: ROOT.into(),
                priority: RunPriority::Normal,
            },
            TOKEN,
        )
        .await
        .unwrap();

    assert_eq!(
        runs.get_recorded(run_id).unwrap().concurrency,
        Some(TEST_CONCURRENCY)
    );
}

#[tokio::test]
async fn given_an_index_that_walks_when_executed_then_the_run_is_recorded_complete() {
    let runs = FakeCatalogRunRepository::new();
    // Same fixture as `given_supported_files_when_execute_then_indexed_with_no_hash_and_indexedat`:
    // two supported, readable files.
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.mp3", "a.mp3", "h-a")
        .with_file(ROOT, "/library/b.md", "b.md", "h-b")
        .build();
    let repo = FakeCatalogRepository::new();
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        runs.clone(),
    );

    let started = handler
        .start(
            IndexRequest {
                root: ROOT.to_string(),
                priority: RunPriority::Normal,
            },
            TOKEN,
        )
        .await
        .expect("start");
    let outcome = handler
        .execute(ROOT, started.run_id)
        .await
        .expect("execute");

    let recorded = runs.get_recorded(started.run_id).expect("run recorded");
    assert_eq!(recorded.status, RunStatus::Complete);
    assert_eq!(
        recorded.counts,
        Some(RunCounts::Index {
            scanned: outcome.scanned,
            indexed: outcome.indexed,
            skipped: outcome.skipped,
            already_cataloged: outcome.already_cataloged,
            failed: outcome.failed,
        }),
        "the recorded tally is the outcome the walk computed"
    );
}

#[tokio::test]
async fn given_a_root_that_cannot_be_listed_when_executed_then_the_run_is_recorded_failed() {
    // FR-FC-27: this is the only case that makes a run `failed` — the walk
    // could not proceed at all.
    let runs = FakeCatalogRunRepository::new();
    let handler = handler(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        FailingListFilesystem,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        runs.clone(),
    );

    let started = handler
        .start(
            IndexRequest {
                root: ROOT.to_string(),
                priority: RunPriority::Normal,
            },
            TOKEN,
        )
        .await
        .expect("start");
    let err = handler
        .execute(ROOT, started.run_id)
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
async fn given_files_that_individually_fail_when_executed_then_the_run_is_complete_not_failed() {
    // FR-FC-27: per-file failures are counted, not escalated. One file whose
    // repository write fails must not report the whole run as failed. Same
    // fixture as
    // `given_failing_repository_write_when_execute_then_run_continues_and_counts_failure`:
    // a.mp3's write fails, b.mp3's does not. (Task 3: an unreadable file no
    // longer produces a per-file failure at index time — see
    // `given_unreadable_file_when_execute_then_it_is_indexed_anyway`.)
    let runs = FakeCatalogRunRepository::new();
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.mp3", "a.mp3", "h-a")
        .with_file(ROOT, "/library/b.mp3", "b.mp3", "h-b")
        .build();
    let repo = FakeCatalogRepository::new().failing_for("/library/a.mp3");
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        runs.clone(),
    );

    let started = handler
        .start(
            IndexRequest {
                root: ROOT.to_string(),
                priority: RunPriority::Normal,
            },
            TOKEN,
        )
        .await
        .expect("start");
    let outcome = handler
        .execute(ROOT, started.run_id)
        .await
        .expect("execute");

    assert!(
        outcome.failed > 0,
        "the fixture must produce a per-file failure"
    );
    let recorded = runs.get_recorded(started.run_id).expect("run recorded");
    assert_eq!(recorded.status, RunStatus::Complete);
}

#[tokio::test]
async fn given_run_completion_cannot_be_recorded_when_executed_then_the_outcome_is_still_returned()
{
    // FR-FC-27: the walk itself succeeds; only the bookkeeping write fails.
    // The caller must still see the outcome it computed — a bookkeeping
    // failure must not sink a completed walk. `IndexHandler::execute` shares
    // this path with `RefreshHandler::execute`
    // (`given_run_completion_cannot_be_recorded_when_executed_then_the_outcome_is_still_returned`
    // in `refresh.rs`); duplicated bodies are exactly the condition under
    // which a future edit to one goes uncaught by the other, so both get a
    // direct regression test. Same fixture as
    // `given_supported_files_when_execute_then_indexed_with_no_hash_and_indexedat`.
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.mp3", "a.mp3", "h-a")
        .with_file(ROOT, "/library/b.md", "b.md", "h-b")
        .build();
    let handler = handler(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FailingCatalogRunRepository::FinishFails,
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("a failed run-completion write must not fail the walk");

    assert_eq!(outcome.scanned, 2);
    assert_eq!(outcome.indexed, 2);
    assert_eq!(outcome.skipped, 0);
    assert_eq!(outcome.failed, 0);
}

/// Requirement A's sharp edge: `execute` now opens with a `get` to read the
/// run's stored concurrency (Task 9), so a transient failure of that one read
/// must not be allowed to sink a walk that could perfectly well run at the
/// configured default — that would be a correctness regression caused by a
/// performance knob. `IndexHandler::execute` shares this path with
/// `RefreshHandler::execute`
/// (`given_the_run_lookup_fails_when_executed_then_the_walk_still_completes_at_the_default_concurrency`
/// in `refresh.rs`); both get a direct test for the same reason the
/// finish-fails case above does.
#[tokio::test]
async fn given_the_run_lookup_fails_when_executed_then_the_walk_still_completes_at_the_default_concurrency(
) {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.mp3", "a.mp3", "h-a")
        .with_file(ROOT, "/library/b.md", "b.md", "h-b")
        .build();
    let handler = handler(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FailingCatalogRunRepository::GetFails,
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("a failed concurrency lookup must not fail the walk");

    assert_eq!(outcome.scanned, 2);
    assert_eq!(outcome.indexed, 2);
    assert_eq!(outcome.skipped, 0);
    assert_eq!(outcome.failed, 0);
}

#[tokio::test]
async fn given_a_cataloged_file_and_an_unsupported_one_when_indexed_then_the_two_are_counted_apart()
{
    let fs = FakeFilesystem::builder()
        .with_root(ROOT)
        .with_file(ROOT, "/library/song.txt", "song.txt", "hash-1")
        .with_file(ROOT, "/library/notes.xyz", "notes.xyz", "hash-2")
        .build();
    let handler = handler(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        FakeCatalogRunRepository::new(),
    );

    // Index once so song.txt is already cataloged, then again over the same
    // root — which is exactly what resume does.
    let IndexStarted { run_id } = handler
        .start(
            IndexRequest {
                root: ROOT.into(),
                priority: RunPriority::Normal,
            },
            TOKEN,
        )
        .await
        .unwrap();
    handler.execute(ROOT, run_id).await.unwrap();

    let IndexStarted { run_id } = handler
        .start(
            IndexRequest {
                root: ROOT.into(),
                priority: RunPriority::Normal,
            },
            TOKEN,
        )
        .await
        .unwrap();
    let outcome = handler.execute(ROOT, run_id).await.unwrap();

    assert_eq!(outcome.indexed, 0);
    assert_eq!(
        outcome.already_cataloged, 1,
        "song.txt is already in the catalog"
    );
    assert_eq!(outcome.skipped, 1, "notes.xyz has an unsupported extension");
}

#[tokio::test]
async fn given_a_completed_index_when_execute_then_the_final_progress_is_flushed_and_the_cell_closed(
) {
    // FR-FC-28: the last thing a run does is publish where it actually
    // finished, so a read after the cell is gone falls back to the truth
    // rather than to whatever the last interval flush caught.
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.mp3", "a.mp3", "unused")
        .with_file(ROOT, "/library/b.txt", "b.txt", "unused")
        .with_file(ROOT, "/library/c.bin", "c.bin", "unused")
        .build();
    let runs = FakeCatalogRunRepository::new();
    let registry = RunRegistry::new();
    let handler = handler_with_registry(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        runs.clone(),
        registry.clone(),
    );
    let run_id = Uuid::new_v4();
    runs.start(run_id, RunKind::Index, Some(ROOT), now(), TEST_CONCURRENCY)
        .await
        .unwrap();

    handler.execute(ROOT, run_id).await.expect("execute");

    let recorded = runs.get_recorded(run_id).expect("recorded run");
    assert_eq!(
        recorded.phase, None,
        "a terminal run has no phase — `complete` and `processing` at once          would tell a client two contradictory things"
    );
    assert_eq!(
        recorded.total,
        Some(3),
        "every entry counts toward the total, and the tally outlives the phase"
    );
    assert_eq!(
        recorded.processed,
        Some(3),
        "an unsupported extension is still an entry the run is done with"
    );
    assert!(
        registry.get(run_id).is_none(),
        "a terminated run must not leave its cell behind"
    );
}

#[tokio::test]
async fn given_a_progress_flush_that_fails_when_execute_then_the_run_still_completes() {
    // FR-FC-28: the in-memory cell is authoritative, so a failed flush costs
    // accuracy after a restart, not correctness. Failing the run over it
    // would throw away work that actually happened.
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.mp3", "a.mp3", "unused")
        .build();
    let runs = FakeCatalogRunRepository::with_failing_progress();
    let handler = handler(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        runs.clone(),
    );
    let run_id = Uuid::new_v4();
    runs.start(run_id, RunKind::Index, Some(ROOT), now(), TEST_CONCURRENCY)
        .await
        .unwrap();

    let outcome = handler.execute(ROOT, run_id).await.expect("execute");

    assert_eq!(outcome.indexed, 1);
    assert!(
        runs.progress_calls() >= 2,
        "both the phase change and the completion must have been attempted"
    );
    let recorded = runs.get_recorded(run_id).expect("recorded run");
    assert_eq!(
        recorded.status,
        RunStatus::Complete,
        "a failed flush must not fail the run"
    );
}

#[tokio::test]
async fn given_a_run_that_cannot_list_its_root_when_execute_then_the_cell_is_closed() {
    // The only failure that aborts a run still has to give the cell back.
    let runs = FakeCatalogRunRepository::new();
    let registry = RunRegistry::new();
    let handler = handler_with_registry(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        FailingListFilesystem,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        runs.clone(),
        registry.clone(),
    );
    let run_id = Uuid::new_v4();
    runs.start(run_id, RunKind::Index, Some(ROOT), now(), TEST_CONCURRENCY)
        .await
        .unwrap();

    handler
        .execute(ROOT, run_id)
        .await
        .expect_err("the walk could not proceed");

    assert!(registry.get(run_id).is_none());
}

#[tokio::test]
async fn given_a_walk_longer_than_the_flush_interval_when_execute_then_intermediate_progress_persists(
) {
    // FR-FC-28: the interval flush is the whole reason the progress columns
    // exist — it is what lets a run this process is no longer executing still
    // report how far it got. Every other test here uses `fixed_clock`, where
    // `now - last_flush` is always zero and this branch never runs, so it
    // needs a clock that actually advances.
    //
    // `SteppingClock` moves one second per read and the loop reads once per
    // entry, so six entries cover six seconds of simulated time and cross the
    // two-second interval three times — deterministically, with no sleeping.
    let mut builder = FakeFilesystem::builder();
    for name in ["a", "b", "c", "d", "e", "f"] {
        builder = builder.with_file(
            ROOT,
            &format!("/library/{name}.mp3"),
            &format!("{name}.mp3"),
            "unused",
        );
    }
    let runs = FakeCatalogRunRepository::new();
    let handler = handler(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        builder.build(),
        SteppingClock::new(now(), 1),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        runs.clone(),
    );
    let run_id = Uuid::new_v4();
    runs.start(run_id, RunKind::Index, Some(ROOT), now(), TEST_CONCURRENCY)
        .await
        .unwrap();

    let outcome = handler.execute(ROOT, run_id).await.expect("execute");

    assert_eq!(outcome.indexed, 6);
    // Each entry of the history is a write the repository actually performed
    // against the row; the row itself only keeps the newest, so this is how a
    // test sees that intermediate values were persisted along the way.
    let history = runs.progress_history();
    assert!(
        history.len() > 2,
        "expected interval flushes on top of the two unconditional ones, got {history:?}"
    );
    let intermediate: Vec<usize> = history
        .iter()
        .map(|progress| progress.processed)
        .filter(|processed| *processed > 0 && *processed < 6)
        .collect();
    assert!(
        !intermediate.is_empty(),
        "no flush landed mid-walk; the interval branch never ran: {history:?}"
    );
    assert!(
        intermediate.windows(2).all(|pair| pair[0] < pair[1]),
        "progress must only ever climb, got {intermediate:?}"
    );
    assert!(
        history
            .iter()
            .all(|progress| progress.total == Some(6) && progress.phase == RunPhase::Processing),
        "every flush after discovery carries the denominator and the phase: {history:?}"
    );
    assert_eq!(
        history.last().map(|progress| progress.processed),
        Some(6),
        "the last flush is the true end state"
    );
}

/// How many entries the pause/cancel walks below are given. Comfortably more
/// than [`TEST_CONCURRENCY`], so that even if every entry already in flight
/// when the signal is raised passes its check, entries are still guaranteed
/// to be left unprocessed.
const HALT_WALK_FILES: usize = 10;

/// A filesystem of `HALT_WALK_FILES` audio files under [`ROOT`], so every
/// entry reaches the audio tag reader — which is where the halt walks below
/// hang their interrupt.
fn audio_library() -> FakeFilesystem {
    let mut builder = FakeFilesystem::builder();
    for n in 0..HALT_WALK_FILES {
        builder = builder.with_file(
            ROOT,
            &format!("/library/{n}.mp3"),
            &format!("{n}.mp3"),
            "unused",
        );
    }
    builder.build()
}

#[derive(Debug, Clone, Copy)]
enum ControlVerb {
    Pause,
    Cancel,
}

fn control_handler(
    runs: FakeCatalogRunRepository,
    registry: RunRegistry,
) -> Arc<RunControlHandler<FakeAuth, FakeCatalogRunRepository, FixedClock>> {
    control_handler_with_low_width(runs, registry, TEST_LOW_PRIORITY_CONCURRENCY)
}

/// [`control_handler`] with `indexing.low_priority_concurrency` set to
/// something of the test's choosing — what a test needs when it resumes at
/// `RunPriority::Low` and wants the resulting width to be a number nothing
/// else in this file could have produced.
fn control_handler_with_low_width(
    runs: FakeCatalogRunRepository,
    registry: RunRegistry,
    low_priority_concurrency: u32,
) -> Arc<RunControlHandler<FakeAuth, FakeCatalogRunRepository, FixedClock>> {
    Arc::new(RunControlHandler::new(
        FakeAuth::Allowing,
        runs,
        fixed_clock(now()),
        registry,
        TEST_CONCURRENCY,
        low_priority_concurrency,
    ))
}

/// An [`Interrupt`] that calls the real `RunControlHandler` — so the walks
/// below are stopped the way a client stops one, not by poking the registry.
fn control_interrupt(
    control: Arc<RunControlHandler<FakeAuth, FakeCatalogRunRepository, FixedClock>>,
    run_id: Uuid,
    verb: ControlVerb,
) -> Interrupt {
    interrupt(move || {
        let control = Arc::clone(&control);
        async move {
            match verb {
                ControlVerb::Pause => control.pause(run_id, TOKEN).await.expect("pause"),
                ControlVerb::Cancel => control.cancel(run_id, TOKEN).await.expect("cancel"),
            }
        }
    })
}

#[tokio::test]
async fn given_an_index_walk_in_flight_when_paused_then_it_stops_with_entries_unprocessed() {
    // The point of pause is not that it flips a status on a run that already
    // ended — it is that it stops one that is still going. The interrupt
    // fires inside the first entry's tag read, so the walk is genuinely under
    // way when the real control handler pauses it, and every entry the loop
    // has not started yet is halted before it does any work.
    let runs = FakeCatalogRunRepository::new();
    let registry = RunRegistry::new();
    let catalog = FakeCatalogRepository::new();
    let run_id = Uuid::new_v4();
    runs.start(run_id, RunKind::Index, Some(ROOT), now(), TEST_CONCURRENCY)
        .await
        .unwrap();
    let pause_mid_walk = control_interrupt(
        control_handler(runs.clone(), registry.clone()),
        run_id,
        ControlVerb::Pause,
    );
    let handler = handler_with_registry(
        FakeAuth::Allowing,
        catalog.clone(),
        audio_library(),
        fixed_clock(now()),
        InterruptingAudioMetadataReader::new(pause_mid_walk),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        runs.clone(),
        registry.clone(),
    );

    let outcome = handler.execute(ROOT, run_id).await.expect("execute");

    assert_eq!(outcome.scanned, HALT_WALK_FILES, "discovery found them all");
    let tallied = outcome.indexed + outcome.skipped + outcome.already_cataloged + outcome.failed;
    assert!(
        tallied > 0,
        "the walk must have got started before it was paused"
    );
    // Deliberate, and the reason `scanned` is not the sum of the rest for a
    // halted run: a halted entry was never processed, so it contributes to no
    // counter. See `EntryOutcome::Halted`.
    assert!(
        tallied < HALT_WALK_FILES,
        "entries must have been left unprocessed, got {tallied} of {HALT_WALK_FILES}"
    );
    assert_eq!(
        catalog.count(),
        outcome.indexed,
        "a halted entry did no work at all — nothing of it reached the catalog"
    );

    let recorded = runs.get_recorded(run_id).expect("recorded run");
    assert_eq!(recorded.status, RunStatus::Paused);
    assert!(recorded.paused_at.is_some());
    assert!(
        recorded.finished_at.is_none(),
        "a paused run has not finished — it can still be resumed"
    );
    assert_eq!(
        recorded.processed,
        Some(tallied),
        "the tally survives the pause, and counts only entries actually processed"
    );
    assert_eq!(
        recorded.total,
        Some(HALT_WALK_FILES),
        "the denominator is what discovery found, not what the walk got through"
    );
    assert_eq!(
        recorded.phase,
        Some(RunPhase::Processing),
        "pause is not terminal, so its phase says where the run stopped"
    );
    assert!(
        recorded.counts.is_none(),
        "a paused run writes no tally: it is resumed and re-walked, so a partial one would be superseded"
    );
    assert!(
        registry.get(run_id).is_none(),
        "a run that stopped must not leave its cell behind"
    );
}

#[tokio::test]
async fn given_a_run_resumed_before_a_walks_pause_lands_when_it_lands_then_the_pause_is_refused() {
    // The gap between `drop(run_cell)` and `record_halt` is a full
    // busy-backoff wide under contention, and both a control-path pause and
    // a resume can land inside it. When they do, the row is `running` again
    // — a *different* segment — and a status guard alone cannot tell that
    // apart from the run this walk was told to pause. Applying the walk's
    // pause there leaves the row `paused` while a live segment is walking
    // it, offers it for resume, and lets a second segment be spawned under
    // one run id.
    let runs = FakeCatalogRunRepository::new();
    let registry = RunRegistry::new();
    let run_id = Uuid::new_v4();
    runs.start(run_id, RunKind::Index, Some(ROOT), now(), TEST_CONCURRENCY)
        .await
        .unwrap();
    let pause_mid_walk = control_interrupt(
        control_handler(runs.clone(), registry.clone()),
        run_id,
        ControlVerb::Pause,
    );
    let handler = handler_with_registry(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        audio_library(),
        fixed_clock(now()),
        InterruptingAudioMetadataReader::new(pause_mid_walk),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        runs.clone(),
        registry.clone(),
    );
    // Someone else pauses and resumes the run in the gap, after the walk has
    // closed its cell and before its own pause is evaluated.
    runs.pause_and_resume_before_next_halt(run_id);

    handler.execute(ROOT, run_id).await.expect("execute");

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
async fn given_a_run_resumed_before_a_walks_cancel_lands_when_it_lands_then_the_cancel_is_refused()
{
    // The same gap as the pause test above, with the worse outcome: `cancel`
    // is terminal. A predecessor segment's late cancel landing on a resumed
    // row marks the run `cancelled` while the new segment keeps walking
    // against it, and the row stays wrong for the whole remaining duration of
    // that scan — only the new segment's unconditional `finish` clears it,
    // minutes later. `TALLY_CANCELLABLE_FROM` admits `running`, so the status
    // guard alone lets it through.
    let runs = FakeCatalogRunRepository::new();
    let registry = RunRegistry::new();
    let run_id = Uuid::new_v4();
    runs.start(run_id, RunKind::Index, Some(ROOT), now(), TEST_CONCURRENCY)
        .await
        .unwrap();
    let cancel_mid_walk = control_interrupt(
        control_handler(runs.clone(), registry.clone()),
        run_id,
        ControlVerb::Cancel,
    );
    let handler = handler_with_registry(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        audio_library(),
        fixed_clock(now()),
        InterruptingAudioMetadataReader::new(cancel_mid_walk),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        runs.clone(),
        registry.clone(),
    );
    // Someone else pauses and resumes the run in the gap, after the walk has
    // closed its cell and before its own cancel is evaluated.
    runs.pause_and_resume_before_next_halt(run_id);

    handler.execute(ROOT, run_id).await.expect("execute");

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
async fn given_an_index_walk_in_flight_when_cancelled_then_it_stops_and_is_terminal() {
    let runs = FakeCatalogRunRepository::new();
    let registry = RunRegistry::new();
    let run_id = Uuid::new_v4();
    runs.start(run_id, RunKind::Index, Some(ROOT), now(), TEST_CONCURRENCY)
        .await
        .unwrap();
    let cancel_mid_walk = control_interrupt(
        control_handler(runs.clone(), registry.clone()),
        run_id,
        ControlVerb::Cancel,
    );
    let handler = handler_with_registry(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        audio_library(),
        fixed_clock(now()),
        InterruptingAudioMetadataReader::new(cancel_mid_walk),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        runs.clone(),
        registry.clone(),
    );

    let outcome = handler.execute(ROOT, run_id).await.expect("execute");

    let tallied = outcome.indexed + outcome.skipped + outcome.already_cataloged + outcome.failed;
    assert!(tallied < HALT_WALK_FILES, "entries were left unprocessed");
    let recorded = runs.get_recorded(run_id).expect("recorded run");
    assert_eq!(recorded.status, RunStatus::Cancelled);
    assert!(
        recorded.finished_at.is_some(),
        "cancel is terminal, so the run has a finish time"
    );
    assert_eq!(
        recorded.phase, None,
        "a terminal run publishes no phase — unlike a paused one"
    );
    assert_eq!(
        recorded.processed,
        Some(tallied),
        "how far a cancelled run got is still worth reporting"
    );
    // A cancelled run is never resumed, so the partial tally it reached is
    // final — kept for the record, exactly as a completed run's is. A pause
    // writes none, because a resumed run supersedes it.
    assert_eq!(
        recorded.counts,
        Some(RunCounts::Index {
            scanned: outcome.scanned,
            indexed: outcome.indexed,
            skipped: outcome.skipped,
            already_cataloged: outcome.already_cataloged,
            failed: outcome.failed,
        }),
        "a cancelled run keeps the four counts the walk computed"
    );
}

#[tokio::test]
async fn given_a_run_paused_during_discovery_when_execute_then_no_entry_is_processed() {
    // `walkdir`'s collect is one blocking call with no interruption point, so
    // discovery is checked exactly once, the moment it returns. A run stopped
    // there has processed nothing, and its phase says so.
    let runs = FakeCatalogRunRepository::new();
    let registry = RunRegistry::new();
    let catalog = FakeCatalogRepository::new();
    let run_id = Uuid::new_v4();
    runs.start(run_id, RunKind::Index, Some(ROOT), now(), TEST_CONCURRENCY)
        .await
        .unwrap();
    let pause_during_discovery = control_interrupt(
        control_handler(runs.clone(), registry.clone()),
        run_id,
        ControlVerb::Pause,
    );
    let handler = handler_with_registry(
        FakeAuth::Allowing,
        catalog.clone(),
        InterruptingFilesystem::new(audio_library(), Seam::Discovery, pause_during_discovery),
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        runs.clone(),
        registry.clone(),
    );

    let outcome = handler.execute(ROOT, run_id).await.expect("execute");

    assert_eq!(
        outcome.scanned, HALT_WALK_FILES,
        "discovery finished; it is the processing loop that never began"
    );
    assert_eq!(outcome.indexed, 0);
    assert_eq!(catalog.count(), 0, "no entry was touched");
    let recorded = runs.get_recorded(run_id).expect("recorded run");
    assert_eq!(recorded.status, RunStatus::Paused);
    assert_eq!(recorded.processed, Some(0));
    assert_eq!(
        recorded.total,
        Some(HALT_WALK_FILES),
        "discovery had already counted them, so the row must not record a NULL total"
    );
    assert_eq!(
        recorded.phase,
        Some(RunPhase::Discovering),
        "a run paused in discovery must be distinguishable from one paused mid-walk"
    );
    assert!(
        recorded.counts.is_none(),
        "a paused run writes no tally — a resume supersedes it"
    );
}

#[tokio::test]
async fn given_a_paused_index_run_when_resumed_then_it_re_walks_and_finishes_the_library() {
    // The owner-visible promise, end to end: a run stopped partway through is
    // picked up again under the same run id and gets through the rest of the
    // library. There is no cursor and no checkpoint — the second segment
    // re-walks the whole root, and the prefix the first one indexed falls out
    // as `already_cataloged` on a single indexed-path lookup each.
    let runs = FakeCatalogRunRepository::new();
    let registry = RunRegistry::new();
    let catalog = FakeCatalogRepository::new();
    let run_id = Uuid::new_v4();
    runs.start(run_id, RunKind::Index, Some(ROOT), now(), TEST_CONCURRENCY)
        .await
        .unwrap();
    let pause_mid_walk = control_interrupt(
        control_handler(runs.clone(), registry.clone()),
        run_id,
        ControlVerb::Pause,
    );
    let first_segment = handler_with_registry(
        FakeAuth::Allowing,
        catalog.clone(),
        audio_library(),
        fixed_clock(now()),
        InterruptingAudioMetadataReader::new(pause_mid_walk),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        runs.clone(),
        registry.clone(),
    );

    let first = first_segment
        .execute(ROOT, run_id)
        .await
        .expect("first segment");
    assert!(
        first.indexed > 0 && first.indexed < HALT_WALK_FILES,
        "the first segment must have got some of the way, not all of it: {first:?}"
    );
    assert_eq!(
        runs.get_recorded(run_id).expect("run").status,
        RunStatus::Paused
    );

    let resumed = control_handler(runs.clone(), registry.clone())
        .resume(run_id, TOKEN, None)
        .await
        .expect("resume");
    assert_eq!(resumed.run_id, run_id);
    assert_eq!(
        resumed.root.as_deref(),
        Some(ROOT),
        "the caller is told which root to spawn the walk over"
    );
    assert_eq!(
        runs.get_recorded(run_id).expect("run").processed,
        Some(0),
        "the resumed segment counts from zero — `processed` was never an offset"
    );

    let second_segment = handler_with_registry(
        FakeAuth::Allowing,
        catalog.clone(),
        audio_library(),
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        runs.clone(),
        registry.clone(),
    );

    let second = second_segment
        .execute(ROOT, run_id)
        .await
        .expect("second segment");

    assert_eq!(
        second.scanned, HALT_WALK_FILES,
        "the whole root is re-walked"
    );
    assert_eq!(
        second.already_cataloged, first.indexed,
        "everything the first segment indexed is re-encountered, and counted as \
         already cataloged rather than skipped"
    );
    assert_eq!(
        second.indexed + second.already_cataloged,
        HALT_WALK_FILES,
        "a client reading `in the library` as indexed + alreadyCataloged lands on \
         the real total"
    );
    assert_eq!(second.failed, 0);
    assert_eq!(
        catalog.count(),
        HALT_WALK_FILES,
        "and every file in the library really is in the catalog now"
    );
    let recorded = runs.get_recorded(run_id).expect("run");
    assert_eq!(
        recorded.status,
        RunStatus::Complete,
        "the resumed run finishes as any other run does"
    );
    assert!(recorded.paused_at.is_none(), "it is not paused any more");
    assert_eq!(
        recorded.counts,
        Some(RunCounts::Index {
            scanned: HALT_WALK_FILES,
            indexed: second.indexed,
            skipped: 0,
            already_cataloged: second.already_cataloged,
            failed: 0,
        }),
        "the recorded tally describes the segment that finished, which is what \
         decision 9 of the design says it should"
    );
}

/// The test the column exists for: a resumed run's second segment walks at
/// the width it was *started* with, not at whatever `IndexHandler` itself was
/// built with. `resumed_at_concurrency` (2) is deliberately distinct from
/// both [`TEST_CONCURRENCY`] (4, what the second segment's handler is built
/// with) and [`TEST_LOW_PRIORITY_CONCURRENCY`] (1), so the assertion below
/// cannot pass by any width coinciding with another by accident.
///
/// Unlike the outcome tallies (`given_any_concurrency_when_execute_...`),
/// which are identical at every width and so cannot distinguish "used the
/// stored value" from "used the field", this reads the actual number of
/// `read` calls a `ConcurrencyTrackingAudioMetadataReader` ever saw in
/// flight together — the one observable that does depend on which width
/// `buffer_unordered` was actually built with.
#[tokio::test]
async fn given_a_resumed_run_when_executed_then_it_walks_at_the_width_it_was_started_at() {
    const RESUMED_AT_CONCURRENCY: u32 = 2;

    let runs = FakeCatalogRunRepository::new();
    let registry = RunRegistry::new();
    let catalog = FakeCatalogRepository::new();
    let run_id = Uuid::new_v4();
    // Started directly against the repository at a width distinct from the
    // second segment's handler field, standing in for a run `IndexHandler`
    // itself started at `RunPriority::Low` some time before this process —
    // `start`'s own write is covered separately by
    // `given_a_low_priority_index_when_started_then_the_run_records_the_low_concurrency`.
    runs.start(
        run_id,
        RunKind::Index,
        Some(ROOT),
        now(),
        RESUMED_AT_CONCURRENCY,
    )
    .await
    .unwrap();
    assert!(runs.pause(run_id, now(), None).await.expect("pause"));

    let resumed = control_handler(runs.clone(), registry.clone())
        .resume(run_id, TOKEN, None)
        .await
        .expect("resume");
    assert_eq!(
        resumed.concurrency, RESUMED_AT_CONCURRENCY,
        "sanity check: the row's own width, not the control handler's fallback"
    );

    let probe = ConcurrencyTrackingAudioMetadataReader::new();
    let second_segment = handler_with_registry(
        FakeAuth::Allowing,
        catalog,
        audio_library(),
        fixed_clock(now()),
        probe.clone(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        runs.clone(),
        registry,
    );

    second_segment
        .execute(ROOT, run_id)
        .await
        .expect("second segment");

    assert_eq!(
        probe.max_seen() as u32,
        RESUMED_AT_CONCURRENCY,
        "the walk must run at the width the run was started at ({RESUMED_AT_CONCURRENCY}), \
         not at the handler's own configured concurrency ({TEST_CONCURRENCY}) — that is what \
         proves execute() reads the stored column rather than falling back to its field"
    );
}

/// Task 15's whole point, end to end: a run started at one width, paused, and
/// resumed at `RunPriority::Low` must have its next segment *actually walked*
/// at the low width. Decision 11 refused a live throttle slider on the
/// promise that a pause and a resume would do the job instead; this is the
/// test that the promise is kept.
///
/// Four distinct widths are in play, deliberately, so no assertion here can
/// pass by coincidence:
///
/// * `STARTED_AT_CONCURRENCY` (2) — the width the run was started at, and
///   what the row would still say if the resume failed to persist the new
///   one;
/// * `RESUMED_AT_LOW_CONCURRENCY` (3) — what `Low` resolves to for the
///   control handler this test builds, and the only correct answer;
/// * [`TEST_CONCURRENCY`] (4) — the second segment's handler field, and what
///   `execute` would fall back to if the width were persisted but not read;
/// * [`TEST_LOW_PRIORITY_CONCURRENCY`] (1) — the file's usual low width,
///   excluded so that "resumed at low" cannot be confused with "some other
///   test's low".
///
/// The observable is the same one Task 9's test uses — the greatest number
/// of `read` calls a `ConcurrencyTrackingAudioMetadataReader` ever saw in
/// flight together — because the outcome tallies are identical at every
/// width and so cannot distinguish one from another.
#[tokio::test]
async fn given_a_paused_run_when_resumed_at_low_priority_then_it_walks_at_the_new_width() {
    const STARTED_AT_CONCURRENCY: u32 = 2;
    const RESUMED_AT_LOW_CONCURRENCY: u32 = 3;

    let runs = FakeCatalogRunRepository::new();
    let registry = RunRegistry::new();
    let catalog = FakeCatalogRepository::new();
    let run_id = Uuid::new_v4();
    runs.start(
        run_id,
        RunKind::Index,
        Some(ROOT),
        now(),
        STARTED_AT_CONCURRENCY,
    )
    .await
    .unwrap();
    assert!(runs.pause(run_id, now(), None).await.expect("pause"));

    let resumed =
        control_handler_with_low_width(runs.clone(), registry.clone(), RESUMED_AT_LOW_CONCURRENCY)
            .resume(run_id, TOKEN, Some(RunPriority::Low))
            .await
            .expect("resume");

    assert_eq!(
        resumed.concurrency, RESUMED_AT_LOW_CONCURRENCY,
        "sanity check: the caller is told the new width, not the old one"
    );
    assert_eq!(
        runs.get_recorded(run_id).expect("run").concurrency,
        Some(RESUMED_AT_LOW_CONCURRENCY),
        "sanity check: and the row records it — persisting it is the only way \
         `execute` can ever see it"
    );

    let probe = ConcurrencyTrackingAudioMetadataReader::new();
    let second_segment = handler_with_registry(
        FakeAuth::Allowing,
        catalog,
        audio_library(),
        fixed_clock(now()),
        probe.clone(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
        runs.clone(),
        registry,
    );

    second_segment
        .execute(ROOT, run_id)
        .await
        .expect("second segment");

    assert_eq!(
        probe.max_seen() as u32,
        RESUMED_AT_LOW_CONCURRENCY,
        "the resumed segment must walk at the width the resume asked for \
         ({RESUMED_AT_LOW_CONCURRENCY}) — not at the width the run was started at \
         ({STARTED_AT_CONCURRENCY}), which is what would show if the new width were \
         answered but never persisted, and not at the handler's own configured \
         concurrency ({TEST_CONCURRENCY}), which is what would show if it were \
         persisted but never read"
    );
}
