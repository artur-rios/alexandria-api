use uuid::Uuid;

use alexandria_core::auth::AuthService;
use alexandria_core::catalog::clock::Clock;
use alexandria_core::catalog::commands::refresh::{RefreshHandler, RefreshStarted};
use alexandria_core::catalog::fs::Filesystem;
use alexandria_core::catalog::repos::CatalogRepository;
use alexandria_core::catalog::run_registry::RunRegistry;
use alexandria_core::catalog::runs::{CatalogRunRepository, RunCounts, RunKind, RunStatus};
use alexandria_core::errors::DomainError;

use crate::common::{
    a_cataloged_file, a_cataloged_file_with_hash, a_cataloged_missing_file, fixed_clock, now,
    FailingCatalogRepository, FailingCatalogRunRepository, FakeAuth, FakeCatalogRepository,
    FakeCatalogRunRepository, FakeFilesystem,
};

const TOKEN: &str = "bearer-token";

/// Deliberately > 1 so these tests exercise the concurrent walk — the outcome
/// tallies must not depend on how many paths are in flight.
const TEST_CONCURRENCY: u32 = 4;

fn refresh_handler<A, R, F, C, RR>(
    auth: A,
    repo: R,
    fs: F,
    clock: C,
    runs: RR,
) -> RefreshHandler<A, R, F, C, RR>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
    C: Clock,
    RR: CatalogRunRepository,
{
    RefreshHandler::new(
        auth,
        repo,
        fs,
        clock,
        TEST_CONCURRENCY,
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
        FakeCatalogRunRepository::new(),
    );

    let result = handler.start("").await;
    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

// ---------------- Stat comparison (Task 4 / FR-FC-10) ----------------

#[tokio::test]
async fn given_an_unchanged_file_when_refreshed_then_it_is_unchanged_and_no_bytes_are_read() {
    let repo = FakeCatalogRepository::new();
    repo.seed(a_cataloged_file("/library/song.flac", 4096, Some(now())));
    let repo_handle = repo.clone();

    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/library/song.flac", "song.flac", "unused")
        .with_stat("/library/song.flac", 4096, Some(now()))
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
            .file_for("/library/song.flac")
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
        "/library/song.flac",
        4096,
        Some(now()),
        "abc",
    ));
    let repo_handle = repo.clone();

    // Same mtime, different size on disk: either one differing is a change.
    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/library/song.flac", "song.flac", "unused")
        .with_stat("/library/song.flac", 8192, Some(now()))
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
    let file = repo_handle.file_for("/library/song.flac").unwrap();
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
        "/library/song.flac",
        4096,
        Some(now()),
        "abc",
    ));
    let repo_handle = repo.clone();

    // Same size, different mtime: either one differing is a change.
    let later = now() + chrono::Duration::seconds(1);
    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/library/song.flac", "song.flac", "unused")
        .with_stat("/library/song.flac", 4096, Some(later))
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
    let file = repo_handle.file_for("/library/song.flac").unwrap();
    assert_eq!(file.mtime, Some(later));
    assert_eq!(fs.hash_calls(), 0, "refresh must not hash");
}

#[tokio::test]
async fn given_disk_missing_path_when_execute_then_marked_missing_record_kept() {
    let repo = FakeCatalogRepository::new();
    repo.seed(a_cataloged_file_with_hash(
        "/library/gone.mp3",
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
        .file_for("/library/gone.mp3")
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
        "/library/back.mp3",
        4096,
        Some(now()),
        "old-hash",
    ));
    let repo_handle = repo.clone();

    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/library/back.mp3", "back.mp3", "unused")
        .with_stat("/library/back.mp3", 4096, Some(now()))
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
        .file_for("/library/back.mp3")
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
        "/library/stillgone.mp3",
        4096,
        Some(now()),
        "old-hash",
    ));
    let repo_handle = repo.clone();
    let prior_missing = repo_handle
        .file_for("/library/stillgone.mp3")
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
    let gone = repo_handle.file_for("/library/stillgone.mp3").unwrap();
    assert_eq!(
        gone.missing_at, prior_missing,
        "missing timestamp not bumped"
    );
}

#[tokio::test]
async fn given_a_repository_write_failure_when_execute_then_refresh_continues_and_counts_failure() {
    let repo = FakeCatalogRepository::new();
    repo.seed(a_cataloged_file_with_hash(
        "/library/a.mp3",
        4096,
        Some(now()),
        "a-old",
    ));
    repo.seed(a_cataloged_file_with_hash(
        "/library/b.mp3",
        4096,
        Some(now()),
        "b-old",
    ));
    let repo = repo.failing_for("/library/a.mp3");
    let repo_handle = repo.clone();

    // Both files changed size on disk, so both would normally be refreshed.
    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/library/a.mp3", "a.mp3", "unused")
        .with_stat("/library/a.mp3", 8192, Some(now()))
        .with_file("/lib", "/library/b.mp3", "b.mp3", "unused")
        .with_stat("/library/b.mp3", 8192, Some(now()))
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
        repo_handle.file_for("/library/b.mp3").unwrap().size_bytes,
        Some(8192)
    );
    assert_eq!(
        repo_handle.file_for("/library/a.mp3").unwrap().size_bytes,
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
            "/library/a.mp3",
            4096,
            Some(now()),
            "a-old",
        ));
        // same stat -> unchanged
        repo.seed(a_cataloged_file_with_hash(
            "/library/b.mp3",
            4096,
            Some(now()),
            "b-same",
        ));
        // absent on disk -> marked missing
        repo.seed(a_cataloged_file_with_hash(
            "/library/c.mp3",
            4096,
            Some(now()),
            "c-old",
        ));
        // present but the write fails -> failed
        repo.seed(a_cataloged_file_with_hash(
            "/library/d.mp3",
            4096,
            Some(now()),
            "d-old",
        ));
        let repo = repo.failing_for("/library/d.mp3");

        let fs = FakeFilesystem::builder()
            .with_file("/lib", "/library/a.mp3", "a.mp3", "unused")
            .with_stat("/library/a.mp3", 8192, Some(now()))
            .with_file("/lib", "/library/b.mp3", "b.mp3", "unused")
            .with_stat("/library/b.mp3", 4096, Some(now()))
            .with_file("/lib", "/library/d.mp3", "d.mp3", "unused")
            .with_stat("/library/d.mp3", 8192, Some(now()))
            .build();
        let handler = RefreshHandler::new(
            FakeAuth::Allowing,
            repo,
            fs,
            fixed_clock(now()),
            concurrency,
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
        "/library/a.mp3",
        4096,
        Some(now()),
        "a-old",
    ));
    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/library/a.mp3", "a.mp3", "unused")
        .with_stat("/library/a.mp3", 8192, Some(now()))
        .build();
    let handler = RefreshHandler::new(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        0,
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
        "/library/a.mp3",
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
        .with_file("/lib", "/library/a.mp3", "a.mp3", "unused")
        .with_stat("/library/a.mp3", 8192, Some(now()))
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
        repo_handle.file_for("/library/a.mp3").unwrap().content_hash,
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

    let started = handler.start(TOKEN).await.expect("start");

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
        "/library/a.mp3",
        4096,
        Some(now()),
        "old-hash",
    ));
    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/library/a.mp3", "a.mp3", "unused")
        .with_stat("/library/a.mp3", 8192, Some(now()))
        .build();
    let handler = refresh_handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        runs.clone(),
    );

    let started = handler.start(TOKEN).await.expect("start");
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

    let started = handler.start(TOKEN).await.expect("start");
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
        "/library/a.mp3",
        4096,
        Some(now()),
        "old-hash",
    ));
    let fs = FakeFilesystem::builder()
        .with_file("/lib", "/library/a.mp3", "a.mp3", "unused")
        .with_stat("/library/a.mp3", 8192, Some(now()))
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

    let result = handler.start(TOKEN).await;

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
    repo.seed(a_cataloged_file("/library/a.mp3", 8192, Some(now())));
    // Absent on disk: it is still an entry the run got through, so it counts
    // toward `processed` like any other.
    repo.seed(a_cataloged_file("/library/b.mp3", 4096, Some(now())));
    let fs = FakeFilesystem::builder()
        .with_stat("/library/a.mp3", 8192, Some(now()))
        .build();
    let runs = FakeCatalogRunRepository::new();
    let registry = RunRegistry::new();
    let handler = RefreshHandler::new(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        TEST_CONCURRENCY,
        runs.clone(),
        registry.clone(),
    );
    let run_id = Uuid::new_v4();
    runs.start(run_id, RunKind::Refresh, None, now())
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
    repo.seed(a_cataloged_file("/library/a.mp3", 8192, Some(now())));
    let fs = FakeFilesystem::builder()
        .with_stat("/library/a.mp3", 8192, Some(now()))
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
    runs.start(run_id, RunKind::Refresh, None, now())
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
