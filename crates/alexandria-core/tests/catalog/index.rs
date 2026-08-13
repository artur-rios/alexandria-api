use uuid::Uuid;

use alexandria_core::auth::AuthService;
use alexandria_core::catalog::audio_tags::{AudioMetadataReader, AudioTags};
use alexandria_core::catalog::classify::classify_by_extension;
use alexandria_core::catalog::clock::Clock;
use alexandria_core::catalog::comic_tags::{ComicMetadataReader, ComicTags};
use alexandria_core::catalog::commands::index::{IndexHandler, IndexRequest, IndexStarted};
use alexandria_core::catalog::document_tags::{DocumentMetadataReader, DocumentTags};
use alexandria_core::catalog::fs::Filesystem;
use alexandria_core::catalog::image_tags::{ImageMetadataReader, ImageTags};
use alexandria_core::catalog::model::{FileType, FormatKind, SubtypeMetadata};
use alexandria_core::catalog::repos::CatalogRepository;
use alexandria_core::catalog::runs::{CatalogRunRepository, RunCounts, RunKind, RunStatus};
use alexandria_core::catalog::video_tags::{VideoDuration, VideoMetadataReader, VideoTags};
use alexandria_core::errors::DomainError;

use crate::common::{
    existing_file, fixed_clock, now, FailingListFilesystem, FakeAudioMetadataReader, FakeAuth,
    FakeCatalogRepository, FakeCatalogRunRepository, FakeComicMetadataReader,
    FakeDocumentMetadataReader, FakeFilesystem, FakeImageMetadataReader, FakeVideoMetadataReader,
};

const ROOT: &str = "/library";
const TOKEN: &str = "bearer-token";

/// The concurrency these unit tests build handlers with. Deliberately > 1:
/// the outcome tallies must not depend on how many entries are in flight, so
/// exercising the concurrent path is what keeps that true. Tests that assert
/// on a *single* entry are unaffected either way.
const TEST_CONCURRENCY: u32 = 4;

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
        library_root,
        runs,
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

    let result = handler
        .start(
            IndexRequest {
                root: "/nope".to_string(),
            },
            TOKEN,
        )
        .await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
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
            },
            "",
        )
        .await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

#[tokio::test]
async fn given_already_cataloged_path_when_execute_then_skipped_no_duplicate() {
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
    assert_eq!(outcome.skipped, 1);
    assert_eq!(
        repo_handle.count(),
        2,
        "exactly the existing + one new record"
    );
    assert!(repo_handle.has_path(existing_path));
    assert!(repo_handle.has_path("/library/b.mp3"));
}

#[tokio::test]
async fn given_supported_files_when_execute_then_indexed_with_hash_and_indexedat() {
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
    assert_eq!(a.content_hash, "h-a");
    assert_eq!(b.content_hash, "h-b");
    assert_eq!(a.indexed_at, now());
    assert_eq!(b.indexed_at, now());
    assert_eq!(a.file_type, FileType::Audio);
    assert_eq!(b.file_type, FileType::Text);
}

// ---------------- Bounded concurrency (FR-FC-08) ----------------

/// The walk processes several files at a time, so the order entries finish in
/// is unspecified — but the tallies are not. Running the same library at every
/// concurrency from sequential to wider-than-the-library must produce
/// identical counts and identical catalog contents.
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
            String::new(),
            FakeCatalogRunRepository::new(),
        );

        let outcome = handler
            .execute(ROOT, Uuid::new_v4())
            .await
            .expect("execute");

        assert_eq!(outcome.scanned, 4, "concurrency {concurrency}");
        assert_eq!(outcome.indexed, 2, "concurrency {concurrency}");
        assert_eq!(
            outcome.skipped, 1,
            "the .zip is unsupported (concurrency {concurrency})"
        );
        assert_eq!(
            outcome.failed, 1,
            "the unreadable mp3 (concurrency {concurrency})"
        );
        assert!(repo_handle.has_path("/library/a.mp3"));
        assert!(repo_handle.has_path("/library/b.md"));
        assert!(!repo_handle.has_path("/library/c.mp3"));
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
        String::new(),
        FakeCatalogRunRepository::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

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

#[tokio::test]
async fn given_unreadable_file_when_execute_then_run_continues_and_counts_failure() {
    // b.mp3 sits between two readable files and cannot be hashed. The run must
    // index a and c anyway rather than aborting at b.
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
    assert_eq!(outcome.indexed, 2, "the two readable files are indexed");
    assert_eq!(
        outcome.failed, 1,
        "the unreadable file is counted as failed"
    );
    assert_eq!(outcome.skipped, 0, "failed is not the same as skipped");
    assert!(repo_handle.has_path("/library/a.mp3"));
    assert!(repo_handle.has_path("/library/c.mp3"));
    assert!(
        !repo_handle.has_path("/library/b.mp3"),
        "the unreadable file is not cataloged"
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
        FakeCatalogRunRepository::new(),
    );
    handler
        .start(
            IndexRequest {
                root: requested.to_string(),
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

    // Act
    let result = start_bounded(&library, &requested).await;

    // Assert
    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
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
    let started = handler.start(IndexRequest { root: requested }, TOKEN).await;

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
    let result = handler.start(IndexRequest { root: requested }, TOKEN).await;

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

#[tokio::test]
async fn given_an_index_that_walks_when_executed_then_the_run_is_recorded_complete() {
    let runs = FakeCatalogRunRepository::new();
    // Same fixture as `given_supported_files_when_execute_then_indexed_with_hash_and_indexedat`:
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
    // FR-FC-27: per-file failures are counted, not escalated. One unreadable
    // file must not report the whole run as failed. Same fixture as
    // `given_unreadable_file_when_execute_then_run_continues_and_counts_failure`:
    // b.mp3 is unreadable, a.mp3 and c.mp3 are not.
    let runs = FakeCatalogRunRepository::new();
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.mp3", "a.mp3", "h-a")
        .with_unreadable_file(ROOT, "/library/b.mp3", "b.mp3")
        .with_file(ROOT, "/library/c.mp3", "c.mp3", "h-c")
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
