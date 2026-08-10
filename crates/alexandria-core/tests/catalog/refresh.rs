use uuid::Uuid;

use alexandria_core::auth::AuthService;
use alexandria_core::catalog::clock::Clock;
use alexandria_core::catalog::commands::refresh::{RefreshHandler, RefreshStarted};
use alexandria_core::catalog::fs::Filesystem;
use alexandria_core::catalog::model::FileType;
use alexandria_core::catalog::repos::CatalogRepository;
use alexandria_core::errors::DomainError;

use crate::common::{
    existing_file_with_hash, existing_missing_file, fixed_clock, now, FakeAuth,
    FakeCatalogRepository, FakeFilesystem,
};

const TOKEN: &str = "bearer-token";

/// Deliberately > 1 so these tests exercise the concurrent walk — the outcome
/// tallies must not depend on how many paths are in flight.
const TEST_CONCURRENCY: u32 = 4;

fn refresh_handler<A, R, F, C>(auth: A, repo: R, fs: F, clock: C) -> RefreshHandler<A, R, F, C>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
    C: Clock,
{
    RefreshHandler::new(auth, repo, fs, clock, TEST_CONCURRENCY)
}

#[tokio::test]
async fn given_authenticated_when_refresh_start_then_returns_run_id() {
    let fs = FakeFilesystem::builder().build();
    let handler = refresh_handler(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        fs,
        fixed_clock(now()),
    );

    let started: RefreshStarted = handler.start(TOKEN).await.expect("start");
    assert_ne!(started.run_id, Uuid::nil());
}

#[tokio::test]
async fn given_unauthenticated_when_refresh_start_then_unauthorized() {
    let fs = FakeFilesystem::builder().build();
    let handler = refresh_handler(
        FakeAuth::Denying,
        FakeCatalogRepository::new(),
        fs,
        fixed_clock(now()),
    );

    let result = handler.start("").await;
    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

#[tokio::test]
async fn given_changed_hash_when_execute_then_hash_and_indexedat_refreshed() {
    let repo = FakeCatalogRepository::new();
    // Cataloged file A with an old hash; the filesystem reports a new hash.
    repo.seed(existing_file_with_hash(
        "/lib/a.mp3",
        "a.mp3",
        FileType::Audio,
        "old-hash",
    ));
    let repo_handle = repo.clone();

    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/lib/a.mp3", "a.mp3", "new-hash")
        .build();
    let handler = refresh_handler(FakeAuth::Allowing, repo, fs, fixed_clock(now()));

    let outcome = handler.execute(Uuid::new_v4()).await.expect("execute");

    assert_eq!(outcome.refreshed, 1);
    assert_eq!(outcome.marked_missing, 0);
    assert_eq!(outcome.unchanged, 0);

    let a = repo_handle
        .file_for("/lib/a.mp3")
        .expect("a still cataloged");
    assert_eq!(a.content_hash, "new-hash");
    assert_eq!(a.indexed_at, now());
    assert!(a.missing_at.is_none(), "refresh clears missing marker");
}

#[tokio::test]
async fn given_unchanged_present_file_when_execute_then_no_write() {
    let repo = FakeCatalogRepository::new();
    repo.seed(existing_file_with_hash(
        "/lib/a.md",
        "a.md",
        FileType::Text,
        "same-hash",
    ));
    let repo_handle = repo.clone();

    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/lib/a.md", "a.md", "same-hash")
        .build();
    let handler = refresh_handler(FakeAuth::Allowing, repo, fs, fixed_clock(now()));

    let outcome = handler.execute(Uuid::new_v4()).await.expect("execute");

    assert_eq!(outcome.refreshed, 0);
    assert_eq!(outcome.marked_missing, 0);
    assert_eq!(outcome.unchanged, 1);

    let a = repo_handle
        .file_for("/lib/a.md")
        .expect("a still cataloged");
    assert_eq!(a.content_hash, "same-hash", "hash untouched");
}

#[tokio::test]
async fn given_disk_missing_path_when_execute_then_marked_missing_record_kept() {
    let repo = FakeCatalogRepository::new();
    repo.seed(existing_file_with_hash(
        "/lib/gone.mp3",
        "gone.mp3",
        FileType::Audio,
        "old-hash",
    ));
    let repo_handle = repo.clone();

    // The file is NOT registered with the filesystem -> path_exists reports false.
    let fs = FakeFilesystem::builder().build();
    let handler = refresh_handler(FakeAuth::Allowing, repo, fs, fixed_clock(now()));

    let outcome = handler.execute(Uuid::new_v4()).await.expect("execute");

    assert_eq!(outcome.marked_missing, 1);
    assert_eq!(outcome.refreshed, 0);
    assert_eq!(outcome.unchanged, 0);

    let gone = repo_handle.file_for("/lib/gone.mp3").expect("record kept");
    assert!(gone.missing_at.is_some(), "missing marker set");
    assert_eq!(
        gone.state,
        alexandria_core::catalog::model::FileState::Active,
        "state is NOT set to deleted (soft-delete is UC-06)"
    );
    assert_eq!(gone.content_hash, "old-hash", "hash untouched when missing");
}

#[tokio::test]
async fn given_missing_file_returned_on_disk_when_execute_then_missing_cleared_and_hash_refreshed()
{
    let repo = FakeCatalogRepository::new();
    // Already marked missing from a prior re-index, with the old hash; the file
    // has since come back with a new hash.
    repo.seed(existing_missing_file(
        "/lib/back.mp3",
        "back.mp3",
        FileType::Audio,
        "old-hash",
    ));
    let repo_handle = repo.clone();

    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/lib/back.mp3", "back.mp3", "returned-hash")
        .build();
    let handler = refresh_handler(FakeAuth::Allowing, repo, fs, fixed_clock(now()));

    let outcome = handler.execute(Uuid::new_v4()).await.expect("execute");

    assert_eq!(outcome.refreshed, 1);
    assert_eq!(outcome.marked_missing, 0);

    let back = repo_handle.file_for("/lib/back.mp3").expect("record kept");
    assert_eq!(back.content_hash, "returned-hash");
    assert!(
        back.missing_at.is_none(),
        "missing marker cleared on return"
    );
    assert_eq!(back.indexed_at, now());
}

#[tokio::test]
async fn given_already_missing_and_still_gone_when_execute_then_left_as_is() {
    let repo = FakeCatalogRepository::new();
    repo.seed(existing_missing_file(
        "/lib/stillgone.mp3",
        "stillgone.mp3",
        FileType::Audio,
        "old-hash",
    ));
    let repo_handle = repo.clone();
    let prior_missing = repo_handle
        .file_for("/lib/stillgone.mp3")
        .unwrap()
        .missing_at;

    let fs = FakeFilesystem::builder().build(); // file still absent
    let handler = refresh_handler(FakeAuth::Allowing, repo, fs, fixed_clock(now()));

    let outcome = handler.execute(Uuid::new_v4()).await.expect("execute");

    assert_eq!(outcome.marked_missing, 0, "no new missing write");
    assert_eq!(outcome.unchanged, 1, "idempotent — left as-is");
    let gone = repo_handle.file_for("/lib/stillgone.mp3").unwrap();
    assert_eq!(
        gone.missing_at, prior_missing,
        "missing timestamp not bumped"
    );
}

#[tokio::test]
async fn given_unreadable_file_when_execute_then_refresh_continues_and_counts_failure() {
    // b is present on disk but unreadable; a is present and changed. The run
    // must still refresh a rather than aborting when b fails to hash.
    let repo = FakeCatalogRepository::new();
    repo.seed(existing_file_with_hash(
        "/lib/a.mp3",
        "a.mp3",
        FileType::Audio,
        "a-old",
    ));
    repo.seed(existing_file_with_hash(
        "/lib/b.mp3",
        "b.mp3",
        FileType::Audio,
        "b-old",
    ));
    let repo_handle = repo.clone();

    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/lib/a.mp3", "a.mp3", "a-new")
        .with_unreadable_file("/lib", "/lib/b.mp3", "b.mp3")
        .build();
    let handler = refresh_handler(FakeAuth::Allowing, repo, fs, fixed_clock(now()));

    let outcome = handler
        .execute(Uuid::new_v4())
        .await
        .expect("an unreadable file must not fail the whole refresh");

    assert_eq!(outcome.refreshed, 1, "a is refreshed despite b failing");
    assert_eq!(outcome.failed, 1);
    assert_eq!(outcome.marked_missing, 0, "b exists — it is not missing");

    assert_eq!(
        repo_handle.file_for("/lib/a.mp3").unwrap().content_hash,
        "a-new"
    );
    assert_eq!(
        repo_handle.file_for("/lib/b.mp3").unwrap().content_hash,
        "b-old",
        "the unreadable file keeps its prior hash"
    );
}

#[tokio::test]
async fn given_failing_repository_write_when_execute_then_refresh_continues_and_counts_failure() {
    let repo = FakeCatalogRepository::new();
    repo.seed(existing_file_with_hash(
        "/lib/a.mp3",
        "a.mp3",
        FileType::Audio,
        "a-old",
    ));
    repo.seed(existing_file_with_hash(
        "/lib/b.mp3",
        "b.mp3",
        FileType::Audio,
        "b-old",
    ));
    let repo = repo.failing_for("/lib/a.mp3");
    let repo_handle = repo.clone();

    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/lib/a.mp3", "a.mp3", "a-new")
        .with_file("/lib", "/lib/b.mp3", "b.mp3", "b-new")
        .build();
    let handler = refresh_handler(FakeAuth::Allowing, repo, fs, fixed_clock(now()));

    let outcome = handler
        .execute(Uuid::new_v4())
        .await
        .expect("a per-file repository error must not fail the whole refresh");

    assert_eq!(outcome.refreshed, 1);
    assert_eq!(outcome.failed, 1);
    assert_eq!(
        repo_handle.file_for("/lib/b.mp3").unwrap().content_hash,
        "b-new"
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
        repo.seed(existing_file_with_hash(
            "/lib/a.mp3",
            "a.mp3",
            FileType::Audio,
            "a-old",
        ));
        // same hash -> unchanged
        repo.seed(existing_file_with_hash(
            "/lib/b.mp3",
            "b.mp3",
            FileType::Audio,
            "b-same",
        ));
        // absent on disk -> marked missing
        repo.seed(existing_file_with_hash(
            "/lib/c.mp3",
            "c.mp3",
            FileType::Audio,
            "c-old",
        ));
        // present but the write fails -> failed
        repo.seed(existing_file_with_hash(
            "/lib/d.mp3",
            "d.mp3",
            FileType::Audio,
            "d-old",
        ));
        let repo = repo.failing_for("/lib/d.mp3");

        let fs = FakeFilesystem::builder()
            .with_file("/lib", "/lib/a.mp3", "a.mp3", "a-new")
            .with_file("/lib", "/lib/b.mp3", "b.mp3", "b-same")
            .with_file("/lib", "/lib/d.mp3", "d.mp3", "d-new")
            .build();
        let handler = RefreshHandler::new(
            FakeAuth::Allowing,
            repo,
            fs,
            fixed_clock(now()),
            concurrency,
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
    repo.seed(existing_file_with_hash(
        "/lib/a.mp3",
        "a.mp3",
        FileType::Audio,
        "a-old",
    ));
    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/lib/a.mp3", "a.mp3", "a-new")
        .build();
    let handler = RefreshHandler::new(FakeAuth::Allowing, repo, fs, fixed_clock(now()), 0);

    let outcome = handler.execute(Uuid::new_v4()).await.expect("execute");

    assert_eq!(outcome.refreshed, 1);
}

#[tokio::test]
async fn given_no_cataloged_files_when_execute_then_empty_outcome() {
    let repo = FakeCatalogRepository::new();
    let fs = FakeFilesystem::builder().build();
    let handler = refresh_handler(FakeAuth::Allowing, repo, fs, fixed_clock(now()));

    let outcome = handler.execute(Uuid::new_v4()).await.expect("execute");
    assert_eq!(outcome.refreshed, 0);
    assert_eq!(outcome.marked_missing, 0);
    assert_eq!(outcome.unchanged, 0);
}

#[tokio::test]
async fn given_mixed_cataloged_files_when_execute_then_each_handled_correctly() {
    // A: changed -> refreshed; B: unchanged present; C: missing on disk.
    let repo = FakeCatalogRepository::new();
    repo.seed(existing_file_with_hash(
        "/lib/a.mp3",
        "a.mp3",
        FileType::Audio,
        "a-old",
    ));
    repo.seed(existing_file_with_hash(
        "/lib/b.md",
        "b.md",
        FileType::Text,
        "b-hash",
    ));
    repo.seed(existing_file_with_hash(
        "/lib/c.pdf",
        "c.pdf",
        FileType::Document,
        "c-hash",
    ));
    let repo_handle = repo.clone();

    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/lib/a.mp3", "a.mp3", "a-new")
        .with_file("/lib", "/lib/b.md", "b.md", "b-hash")
        // c.pdf absent on disk
        .build();
    let handler = refresh_handler(FakeAuth::Allowing, repo, fs, fixed_clock(now()));

    let outcome = handler.execute(Uuid::new_v4()).await.expect("execute");

    assert_eq!(outcome.refreshed, 1);
    assert_eq!(outcome.marked_missing, 1);
    assert_eq!(outcome.unchanged, 1);

    assert_eq!(
        repo_handle.file_for("/lib/a.mp3").unwrap().content_hash,
        "a-new"
    );
    assert_eq!(
        repo_handle.file_for("/lib/b.md").unwrap().content_hash,
        "b-hash"
    );
    assert!(repo_handle
        .file_for("/lib/c.pdf")
        .unwrap()
        .missing_at
        .is_some());
}
