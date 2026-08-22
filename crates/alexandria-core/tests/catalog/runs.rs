//! Tests for the UC-42 run record: the SQLite repository against a real
//! migrated database (Testing Specification §6.4), covering each lifecycle
//! transition and the startup reconciliation FR-FC-29 requires.

use chrono::{TimeZone, Utc};
use uuid::Uuid;

use alexandria_core::catalog::run_registry::{RunPhase, RunProgress};
use alexandria_core::catalog::runs::{
    CatalogRunRepository, RunCounts, RunKind, RunStatus, SqliteCatalogRunRepository,
};
use alexandria_core::migrate::migrate_database;

async fn repo() -> (SqliteCatalogRunRepository, tempfile::TempDir) {
    let (repo, _pool, dir) = repo_with_pool().await;
    (repo, dir)
}

/// As [`repo`], but keeping the pool as well - for the columns real SQL reads
/// back that no command writes yet (`concurrency`, which run priority will
/// write), and for seeding a pause directly rather than through the
/// transition that produces it, so the read path is covered on its own.
async fn repo_with_pool() -> (
    SqliteCatalogRunRepository,
    sqlx::sqlite::SqlitePool,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("alexandria.sqlite");
    let pool = migrate_database(path.to_str().expect("path"))
        .await
        .expect("migrate");
    (SqliteCatalogRunRepository::new(pool.clone()), pool, dir)
}

fn t(hour: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, hour, 0, 0).unwrap()
}

#[tokio::test]
async fn given_a_started_run_when_read_then_it_is_running_with_no_counts() {
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();

    repo.start(id, RunKind::Index, Some("/library"), t(1))
        .await
        .expect("start");

    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.id, id);
    assert_eq!(run.kind, RunKind::Index);
    assert_eq!(run.status, RunStatus::Running);
    assert_eq!(run.root.as_deref(), Some("/library"));
    assert_eq!(run.started_at, t(1));
    assert!(run.finished_at.is_none(), "a running run has not finished");
    assert!(run.counts.is_none(), "no tally exists until the walk ends");
    assert!(run.error.is_none());
}

#[tokio::test]
async fn given_a_running_index_run_when_finished_then_it_is_complete_with_its_counts() {
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Index, Some("/library"), t(1))
        .await
        .expect("start");

    repo.finish(
        id,
        RunCounts::Index {
            scanned: 13,
            indexed: 7,
            skipped: 2,
            already_cataloged: 3,
            failed: 1,
        },
        t(2),
    )
    .await
    .expect("finish");

    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.status, RunStatus::Complete);
    assert_eq!(run.finished_at, Some(t(2)));
    assert_eq!(
        run.counts,
        Some(RunCounts::Index {
            scanned: 13,
            indexed: 7,
            skipped: 2,
            already_cataloged: 3,
            failed: 1
        })
    );
    assert!(run.error.is_none());
}

#[tokio::test]
async fn given_a_run_with_per_file_failures_when_finished_then_it_is_complete_not_failed() {
    // FR-FC-27: one unreadable file must not make the whole run a failure.
    // `failed` counts them; the run still completed its walk.
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Refresh, None, t(1))
        .await
        .expect("start");

    repo.finish(
        id,
        RunCounts::Refresh {
            refreshed: 1,
            marked_missing: 0,
            unchanged: 3,
            failed: 5,
        },
        t(2),
    )
    .await
    .expect("finish");

    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(
        run.status,
        RunStatus::Complete,
        "per-file failures do not make the run failed"
    );
}

#[tokio::test]
async fn given_a_running_refresh_run_when_failed_then_it_carries_the_error() {
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Refresh, None, t(1))
        .await
        .expect("start");

    repo.fail(id, "catalog unreadable", t(2))
        .await
        .expect("fail");

    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.error.as_deref(), Some("catalog unreadable"));
    assert_eq!(run.finished_at, Some(t(2)));
    assert!(
        run.counts.is_none(),
        "a run that could not proceed has no tally"
    );
}

#[tokio::test]
async fn given_a_refresh_run_when_started_then_it_has_no_root() {
    // A refresh touches every cataloged path and takes no root.
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Refresh, None, t(1))
        .await
        .expect("start");

    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.kind, RunKind::Refresh);
    assert!(run.root.is_none());
}

#[tokio::test]
async fn given_an_unknown_id_when_read_then_none() {
    // UC-42 AF-01.
    let (repo, _dir) = repo().await;
    assert!(repo.get(Uuid::new_v4()).await.expect("get").is_none());
}

#[tokio::test]
async fn given_running_and_terminal_runs_when_reconciled_then_only_running_becomes_paused() {
    // FR-FC-29: a row still `running` at startup provably has no task behind
    // it, because runs execute in-process. It becomes `paused` rather than
    // terminal, so closing the application mid-scan leaves work to resume
    // rather than work to redo. Terminal rows must be left exactly as they
    // are.
    let (repo, _dir) = repo().await;
    let running = Uuid::new_v4();
    let completed = Uuid::new_v4();
    let failed = Uuid::new_v4();

    repo.start(running, RunKind::Index, Some("/library"), t(1))
        .await
        .expect("start running");
    repo.start(completed, RunKind::Refresh, None, t(1))
        .await
        .expect("start completed");
    repo.finish(
        completed,
        RunCounts::Refresh {
            refreshed: 1,
            marked_missing: 0,
            unchanged: 0,
            failed: 0,
        },
        t(2),
    )
    .await
    .expect("finish");
    repo.start(failed, RunKind::Refresh, None, t(1))
        .await
        .expect("start failed");
    repo.fail(failed, "catalog unreadable", t(2))
        .await
        .expect("fail");

    let reconciled = repo.pause_running(t(3)).await.expect("pause running");

    assert_eq!(reconciled, 1, "only the running row is reconciled");
    let run = repo.get(running).await.unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Paused);
    assert_eq!(run.paused_at, Some(t(3)));
    assert!(
        run.finished_at.is_none(),
        "a run offered for resume has not finished"
    );
    assert_eq!(
        repo.get(completed).await.unwrap().unwrap().status,
        RunStatus::Complete
    );
    assert_eq!(
        repo.get(failed).await.unwrap().unwrap().status,
        RunStatus::Failed
    );
}

#[tokio::test]
async fn given_no_running_runs_when_reconciled_then_nothing_changes() {
    let (repo, _dir) = repo().await;
    assert_eq!(repo.pause_running(t(3)).await.expect("pause running"), 0);
}

#[tokio::test]
async fn given_a_run_still_marked_running_at_startup_when_reconciled_then_it_is_paused_not_lost() {
    // FR-FC-29 end to end: a run recorded as running by a previous process is
    // reconciled at startup into a run the owner is *offered*, not a loss they
    // are informed of. Nothing starts by itself — resuming is an explicit act.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("alexandria.sqlite");
    let pool = migrate_database(path.to_str().expect("path"))
        .await
        .expect("migrate");

    let repo = SqliteCatalogRunRepository::new(pool.clone());
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Refresh, None, t(1))
        .await
        .expect("start");

    let _services =
        alexandria_core::services::build_services(&Default::default(), pool.clone()).await;

    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.status, RunStatus::Paused);
    assert!(
        run.finished_at.is_none(),
        "the run is resumable, so it carries no finish time"
    );
}

#[tokio::test]
async fn given_an_index_run_when_finished_with_refresh_counts_then_error_and_row_untouched() {
    // Passing the wrong kind's tally would leave the row's real (index)
    // columns NULL and the wrong-kind columns set — a corrupted write that
    // `get` would otherwise report as a `Complete` run with no counts.
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Index, Some("/library"), t(1))
        .await
        .expect("start");

    let result = repo
        .finish(
            id,
            RunCounts::Refresh {
                refreshed: 1,
                marked_missing: 0,
                unchanged: 0,
                failed: 0,
            },
            t(2),
        )
        .await;

    assert!(result.is_err(), "mismatched counts kind must be rejected");
    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.status, RunStatus::Running, "the row is left untouched");
    assert!(run.counts.is_none());
    assert!(run.finished_at.is_none());
}

#[tokio::test]
async fn given_a_refresh_run_when_finished_with_index_counts_then_error_and_row_untouched() {
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Refresh, None, t(1))
        .await
        .expect("start");

    let result = repo
        .finish(
            id,
            RunCounts::Index {
                scanned: 1,
                indexed: 1,
                skipped: 0,
                already_cataloged: 0,
                failed: 0,
            },
            t(2),
        )
        .await;

    assert!(result.is_err(), "mismatched counts kind must be rejected");
    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.status, RunStatus::Running, "the row is left untouched");
    assert!(run.counts.is_none());
    assert!(run.finished_at.is_none());
}

#[tokio::test]
async fn given_a_running_index_run_when_serialized_then_only_running_fields_present() {
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Index, Some("/library"), t(1))
        .await
        .expect("start");
    let run = repo.get(id).await.expect("get").expect("run exists");

    let value = serde_json::to_value(&run).expect("serialize");

    assert_eq!(
        value.get("runId").and_then(|v| v.as_str()),
        Some(id.to_string().as_str())
    );
    assert_eq!(value.get("kind").and_then(|v| v.as_str()), Some("index"));
    assert_eq!(
        value.get("status").and_then(|v| v.as_str()),
        Some("running")
    );
    assert_eq!(value.get("root").and_then(|v| v.as_str()), Some("/library"));
    assert!(value.get("startedAt").is_some());
    assert!(
        value.get("finishedAt").is_none(),
        "a running run has not finished"
    );
    assert!(value.get("scanned").is_none());
    assert!(value.get("indexed").is_none());
    assert!(value.get("skipped").is_none());
    assert!(value.get("alreadyCataloged").is_none());
    assert!(value.get("failed").is_none());
    assert!(value.get("error").is_none());
}

#[tokio::test]
async fn given_a_completed_refresh_run_when_serialized_then_counts_are_flattened_top_level() {
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Refresh, None, t(1))
        .await
        .expect("start");
    repo.finish(
        id,
        RunCounts::Refresh {
            refreshed: 1,
            marked_missing: 2,
            unchanged: 3,
            failed: 4,
        },
        t(2),
    )
    .await
    .expect("finish");
    let run = repo.get(id).await.expect("get").expect("run exists");

    let value = serde_json::to_value(&run).expect("serialize");

    assert_eq!(value.get("refreshed").and_then(|v| v.as_u64()), Some(1));
    assert_eq!(value.get("markedMissing").and_then(|v| v.as_u64()), Some(2));
    assert_eq!(value.get("unchanged").and_then(|v| v.as_u64()), Some(3));
    assert_eq!(value.get("failed").and_then(|v| v.as_u64()), Some(4));
    assert!(value.get("finishedAt").is_some());
    assert!(value.get("scanned").is_none());
    assert!(value.get("indexed").is_none());
    assert!(value.get("skipped").is_none());
    assert!(value.get("alreadyCataloged").is_none());
    assert!(value.get("root").is_none(), "a refresh has no root");
    assert!(value.get("error").is_none());
}

#[tokio::test]
async fn given_a_completed_index_run_when_serialized_then_counts_and_root_are_flattened_top_level()
{
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Index, Some("/library"), t(1))
        .await
        .expect("start");
    repo.finish(
        id,
        RunCounts::Index {
            scanned: 13,
            indexed: 7,
            skipped: 2,
            already_cataloged: 3,
            failed: 1,
        },
        t(2),
    )
    .await
    .expect("finish");
    let run = repo.get(id).await.expect("get").expect("run exists");

    let value = serde_json::to_value(&run).expect("serialize");

    assert_eq!(value.get("root").and_then(|v| v.as_str()), Some("/library"));
    assert_eq!(value.get("scanned").and_then(|v| v.as_u64()), Some(13));
    assert_eq!(value.get("indexed").and_then(|v| v.as_u64()), Some(7));
    assert_eq!(value.get("skipped").and_then(|v| v.as_u64()), Some(2));
    assert_eq!(
        value.get("alreadyCataloged").and_then(|v| v.as_u64()),
        Some(3)
    );
    assert_eq!(value.get("failed").and_then(|v| v.as_u64()), Some(1));
    assert!(value.get("finishedAt").is_some());
    // Refresh-only count keys must be absent, not sent as null.
    assert!(value.get("refreshed").is_none());
    assert!(value.get("markedMissing").is_none());
    assert!(value.get("unchanged").is_none());
    assert!(value.get("error").is_none());
}

#[tokio::test]
async fn given_a_failed_run_when_serialized_then_error_present_and_no_counts() {
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Refresh, None, t(1))
        .await
        .expect("start");
    repo.fail(id, "catalog unreadable", t(2))
        .await
        .expect("fail");
    let run = repo.get(id).await.expect("get").expect("run exists");

    let value = serde_json::to_value(&run).expect("serialize");

    assert_eq!(
        value.get("error").and_then(|v| v.as_str()),
        Some("catalog unreadable")
    );
    assert_eq!(value.get("status").and_then(|v| v.as_str()), Some("failed"));
    assert!(value.get("finishedAt").is_some());
    assert!(value.get("refreshed").is_none());
    assert!(value.get("markedMissing").is_none());
    assert!(value.get("unchanged").is_none());
    assert!(value.get("scanned").is_none());
    assert!(value.get("indexed").is_none());
    assert!(value.get("skipped").is_none());
    assert!(value.get("alreadyCataloged").is_none());
    assert!(
        value.get("failed").is_none(),
        "no failed-count key on a run with no tally"
    );
}

#[tokio::test]
async fn given_a_running_run_when_progress_is_recorded_then_it_reads_back_from_the_row() {
    // FR-FC-28: the flush is the only durable record of how far a run got, so
    // its own UPDATE and the `get` that reads it have to round-trip against
    // real SQL, not just against the in-memory fake.
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Index, Some("/library"), t(1))
        .await
        .expect("start");

    repo.record_progress(
        id,
        &RunProgress {
            phase: RunPhase::Processing,
            total: Some(12_264),
            processed: 8_412,
        },
    )
    .await
    .expect("record progress");

    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.phase, Some(RunPhase::Processing));
    assert_eq!(run.total, Some(12_264));
    assert_eq!(run.processed, Some(8_412));
    assert_eq!(
        run.status,
        RunStatus::Running,
        "a flush is not a transition"
    );
    assert_eq!(
        run.paused_millis, 0,
        "a run that was never paused needs no special case"
    );
    assert!(run.paused_at.is_none());
}

#[tokio::test]
async fn given_a_run_still_discovering_when_progress_is_recorded_then_the_total_is_null() {
    // Discovery has no denominator yet: the sentinel must reach the row as
    // NULL and read back as `None`, not as some large number.
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Index, Some("/library"), t(1))
        .await
        .expect("start");

    repo.record_progress(
        id,
        &RunProgress {
            phase: RunPhase::Discovering,
            total: None,
            processed: 0,
        },
    )
    .await
    .expect("record progress");

    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.phase, Some(RunPhase::Discovering));
    assert_eq!(run.total, None);
    assert_eq!(run.processed, Some(0));
}

#[tokio::test]
async fn given_a_run_with_progress_when_finished_then_the_phase_clears_and_the_tally_stays() {
    // A terminal run has no phase: `status = complete` beside
    // `phase = processing` would tell a client two contradictory things. The
    // numbers are still true, so they stay.
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Index, Some("/library"), t(1))
        .await
        .expect("start");
    repo.record_progress(
        id,
        &RunProgress {
            phase: RunPhase::Processing,
            total: Some(13),
            processed: 13,
        },
    )
    .await
    .expect("record progress");

    repo.finish(
        id,
        RunCounts::Index {
            scanned: 13,
            indexed: 7,
            skipped: 2,
            already_cataloged: 3,
            failed: 1,
        },
        t(2),
    )
    .await
    .expect("finish");

    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.status, RunStatus::Complete);
    assert_eq!(run.phase, None, "a terminal run publishes no phase");
    assert_eq!(run.total, Some(13));
    assert_eq!(run.processed, Some(13));
    let value = serde_json::to_value(&run).expect("serialize");
    assert!(
        value.get("phase").is_none(),
        "an absent phase is omitted, not sent as null"
    );
}

#[tokio::test]
async fn given_a_run_with_progress_when_it_fails_then_the_phase_clears() {
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Refresh, None, t(1))
        .await
        .expect("start");
    repo.record_progress(
        id,
        &RunProgress {
            phase: RunPhase::Processing,
            total: Some(4),
            processed: 1,
        },
    )
    .await
    .expect("record progress");

    repo.fail(id, "catalog unreadable", t(2))
        .await
        .expect("fail");

    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.phase, None);
    assert_eq!(
        run.processed,
        Some(1),
        "how far a failed run got is still worth reporting"
    );
}

#[tokio::test]
async fn given_a_run_reconciled_at_startup_when_read_then_it_publishes_no_phase() {
    // A run paused by startup reconciliation is not *in* a phase: its process
    // is gone, and it will not be in one again until it is resumed. That is
    // the one thing separating it from a run an owner paused, whose phase says
    // where its still-live walk stopped.
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Index, Some("/library"), t(1))
        .await
        .expect("start");
    repo.record_progress(
        id,
        &RunProgress {
            phase: RunPhase::Processing,
            total: Some(9),
            processed: 5,
        },
    )
    .await
    .expect("record progress");

    repo.pause_running(t(2)).await.expect("pause running");

    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.status, RunStatus::Paused);
    assert_eq!(run.paused_at, Some(t(2)));
    assert_eq!(run.phase, None);
    assert_eq!(
        run.processed,
        Some(5),
        "the last flush is what the lost segment has to show for itself"
    );
}

#[tokio::test]
async fn given_a_running_run_with_progress_when_paused_then_it_keeps_its_phase_and_stamps_paused_at(
) {
    // Pause is the one non-terminal transition, so it is the one that keeps
    // its phase: a client reading `paused` beside `processing` learns the run
    // stopped mid-walk rather than mid-discovery, which is exactly what it
    // needs to know before resuming.
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Index, Some("/library"), t(1))
        .await
        .expect("start");
    repo.record_progress(
        id,
        &RunProgress {
            phase: RunPhase::Processing,
            total: Some(9),
            processed: 4,
        },
    )
    .await
    .expect("record progress");

    let applied = repo.pause(id, t(2)).await.expect("pause");

    assert!(applied, "a running run accepts a pause");
    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.status, RunStatus::Paused);
    assert_eq!(run.paused_at, Some(t(2)));
    assert_eq!(
        run.phase,
        Some(RunPhase::Processing),
        "a paused run is not terminal, so its phase still describes where it stopped"
    );
    assert_eq!(run.processed, Some(4), "the tally survives the pause");
    assert!(
        run.finished_at.is_none(),
        "a paused run has not finished — it can still be resumed"
    );
    let value = serde_json::to_value(&run).expect("serialize");
    assert_eq!(value.get("status").and_then(|v| v.as_str()), Some("paused"));
    assert!(value.get("pausedAt").is_some());
}

#[tokio::test]
async fn given_a_run_with_progress_when_cancelled_then_it_is_terminal_and_clears_its_phase() {
    // Cancel is terminal, so it clears the phase for the same reason `finish`
    // and `fail` do: `status = cancelled` beside `phase = processing` would
    // tell a client two contradictory things.
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Refresh, None, t(1))
        .await
        .expect("start");
    repo.record_progress(
        id,
        &RunProgress {
            phase: RunPhase::Processing,
            total: Some(9),
            processed: 4,
        },
    )
    .await
    .expect("record progress");

    repo.cancel(id, None, t(2)).await.expect("cancel");

    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.status, RunStatus::Cancelled);
    assert_eq!(run.finished_at, Some(t(2)));
    assert_eq!(run.phase, None, "a terminal run publishes no phase");
    assert_eq!(
        run.processed,
        Some(4),
        "how far a cancelled run got is still worth reporting"
    );
    let value = serde_json::to_value(&run).expect("serialize");
    assert_eq!(
        value.get("status").and_then(|v| v.as_str()),
        Some("cancelled")
    );
}

#[tokio::test]
async fn given_a_paused_run_when_startup_reconciles_then_it_is_left_paused() {
    // FR-FC-29 reconciles `running` rows, and a paused run must not be one of
    // them: it stopped deliberately, and its row is what a later resume has
    // to work from.
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Index, Some("/library"), t(1))
        .await
        .expect("start");
    assert!(repo.pause(id, t(2)).await.expect("pause"));

    let reconciled = repo.pause_running(t(3)).await.expect("pause running");

    assert_eq!(reconciled, 0, "a paused run is not a running one");
    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.status, RunStatus::Paused);
    assert_eq!(run.paused_at, Some(t(2)));
}

#[tokio::test]
async fn given_a_row_with_pause_columns_set_when_read_then_they_come_back_off_the_row() {
    // `paused_at` / `paused_millis` are read by real SQL here and written by
    // the pause/resume command in a later task. Seeded directly so the read
    // path - including the RFC 3339 parse of `paused_at` - is covered now
    // rather than resting on a test-only fake setter.
    let (repo, pool, _dir) = repo_with_pool().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Index, Some("/library"), t(1))
        .await
        .expect("start");

    sqlx::query("UPDATE catalog_runs SET paused_at = ?, paused_millis = ? WHERE id = ?")
        .bind(t(2).to_rfc3339())
        .bind(90_000_i64)
        .bind(id.to_string())
        .execute(&pool)
        .await
        .expect("seed the pause columns");

    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.paused_at, Some(t(2)));
    assert_eq!(run.paused_millis, 90_000);
    let value = serde_json::to_value(&run).expect("serialize");
    assert!(
        value.get("pausedMillis").is_none(),
        "pausedMillis is the input activeMillis is derived from, not a client field"
    );
}

#[tokio::test]
async fn given_a_cancelled_run_when_a_pause_write_lands_afterwards_then_the_cancel_stands() {
    // The window the `AND status = 'running'` guard exists for. A walk closes
    // its cell *before* its own terminal write; a cancel arriving in that gap
    // finds no live cell and writes the row directly, and the walk's own
    // `pause` then lands second. Unguarded, that pause would leave `paused`
    // beside the `finished_at` the cancel stamped, and a run the owner asked
    // to abandon would look resumable.
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Index, Some("/library"), t(1))
        .await
        .expect("start");
    repo.cancel(id, None, t(2)).await.expect("cancel");

    let applied = repo.pause(id, t(3)).await.expect("pause");

    assert!(!applied, "a pause must not apply to a run already closed");
    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.status, RunStatus::Cancelled, "the cancel stands");
    assert_eq!(run.finished_at, Some(t(2)));
    assert!(
        run.paused_at.is_none(),
        "the refused pause must not have stamped a pause time either"
    );
}

#[tokio::test]
async fn given_a_completed_run_when_a_pause_write_lands_afterwards_then_it_is_refused() {
    // The same guard seen from the ordinary direction: a walk that finished
    // between a control call's lookup and its write.
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Refresh, None, t(1))
        .await
        .expect("start");
    repo.finish(
        id,
        RunCounts::Refresh {
            refreshed: 1,
            marked_missing: 0,
            unchanged: 0,
            failed: 0,
        },
        t(2),
    )
    .await
    .expect("finish");

    let applied = repo.pause(id, t(3)).await.expect("pause");

    assert!(!applied);
    assert_eq!(
        repo.get(id).await.unwrap().unwrap().status,
        RunStatus::Complete
    );
}

#[tokio::test]
async fn given_a_cancelled_run_with_a_tally_when_read_then_the_counts_are_kept() {
    // A cancelled run is never resumed, so the tally it reached is final —
    // and a client deserves the same four numbers a completed run gives it,
    // not just `processed` from the last flush.
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Index, Some("/library"), t(1))
        .await
        .expect("start");

    repo.cancel(
        id,
        Some(RunCounts::Index {
            scanned: 13,
            indexed: 4,
            skipped: 1,
            already_cataloged: 0,
            failed: 1,
        }),
        t(2),
    )
    .await
    .expect("cancel");

    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.status, RunStatus::Cancelled);
    assert_eq!(
        run.counts,
        Some(RunCounts::Index {
            scanned: 13,
            indexed: 4,
            skipped: 1,
            already_cataloged: 0,
            failed: 1
        })
    );
    assert_eq!(run.phase, None, "still terminal, so still no phase");
    let value = serde_json::to_value(&run).expect("serialize");
    assert_eq!(value.get("scanned").and_then(|v| v.as_u64()), Some(13));
    assert_eq!(value.get("indexed").and_then(|v| v.as_u64()), Some(4));
    assert!(
        value.get("scanned").and_then(|v| v.as_u64())
            > value.get("indexed").and_then(|v| v.as_u64()),
        "a cancelled run's scanned exceeds what it processed — that is the point"
    );
}

#[tokio::test]
async fn given_an_index_run_when_cancelled_with_refresh_counts_then_error_and_row_untouched() {
    // Cancel keeps a tally now, so it inherits `finish`'s kind guard: writing
    // the wrong variant would leave the row's real columns NULL.
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Index, Some("/library"), t(1))
        .await
        .expect("start");

    let result = repo
        .cancel(
            id,
            Some(RunCounts::Refresh {
                refreshed: 1,
                marked_missing: 0,
                unchanged: 0,
                failed: 0,
            }),
            t(2),
        )
        .await;

    assert!(result.is_err(), "mismatched counts kind must be rejected");
    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.status, RunStatus::Running, "the row is left untouched");
    assert!(run.counts.is_none());
    assert!(run.finished_at.is_none());
}

#[tokio::test]
async fn given_a_paused_run_with_progress_when_resumed_then_the_segment_counters_reset() {
    // Resume re-walks from the start; it does not seek to an offset.
    // `processed` is a count of what one segment folded, never a position in
    // the walk, so a resumed segment that inherited it would report a run
    // further along than it is. Clearing `total` and returning to
    // `discovering` says the same about the denominator: the resumed segment
    // counts it again for itself.
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Index, Some("/library"), t(1))
        .await
        .expect("start");
    repo.record_progress(
        id,
        &RunProgress {
            phase: RunPhase::Processing,
            total: Some(12_264),
            processed: 8_412,
        },
    )
    .await
    .expect("record progress");
    assert!(repo.pause(id, t(2)).await.expect("pause"));

    let applied = repo.resume(id, 90_000).await.expect("resume");

    assert!(applied, "a paused run accepts a resume");
    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.status, RunStatus::Running);
    assert_eq!(run.paused_at, None, "the run is no longer paused");
    assert_eq!(
        run.paused_millis, 90_000,
        "the banked total is what was set"
    );
    assert_eq!(run.processed, Some(0), "the segment's counter restarts");
    assert_eq!(run.total, None, "the denominator is rediscovered");
    assert_eq!(
        run.phase,
        Some(RunPhase::Discovering),
        "a resumed run starts where every run starts"
    );
    assert!(run.finished_at.is_none());
}

#[tokio::test]
async fn given_a_run_that_is_not_paused_when_resumed_then_the_write_is_refused() {
    // The same shape of guard `pause` and `cancel` carry: the row must still
    // be in the state the caller decided from. A resume landing on a cancelled
    // run would revive a run its owner abandoned.
    let (repo, _dir) = repo().await;
    let running = Uuid::new_v4();
    repo.start(running, RunKind::Index, Some("/library"), t(1))
        .await
        .expect("start");
    let cancelled = Uuid::new_v4();
    repo.start(cancelled, RunKind::Index, Some("/library"), t(1))
        .await
        .expect("start");
    repo.cancel(cancelled, None, t(2)).await.expect("cancel");

    assert!(
        !repo.resume(running, 0).await.expect("resume"),
        "a running run is already running"
    );
    assert!(
        !repo.resume(cancelled, 0).await.expect("resume"),
        "a cancelled run must not be revived"
    );
    let run = repo.get(cancelled).await.unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Cancelled);
    assert_eq!(
        run.finished_at,
        Some(t(2)),
        "and the refused write left its finish time alone"
    );
}

#[tokio::test]
async fn given_an_unknown_run_when_resumed_then_the_write_is_refused() {
    let (repo, _dir) = repo().await;
    assert!(!repo.resume(Uuid::new_v4(), 0).await.expect("resume"));
}

#[tokio::test]
async fn given_a_completed_run_when_a_cancel_write_lands_afterwards_then_the_completion_stands() {
    // The cancel-side mirror of the pause guard. A control call reads
    // `running`, the walk then writes `finish`, and the cancel lands last --
    // rewriting a run that completed all of its work into a `cancelled` one
    // with a fresh `finished_at`, and telling the caller it succeeded. The row
    // stays internally coherent, which is exactly why nothing else catches it.
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Index, Some("/library"), t(1))
        .await
        .expect("start");
    repo.finish(
        id,
        RunCounts::Index {
            scanned: 13,
            indexed: 13,
            skipped: 0,
            already_cataloged: 0,
            failed: 0,
        },
        t(2),
    )
    .await
    .expect("finish");

    let applied = repo.cancel(id, None, t(3)).await.expect("cancel");

    assert!(!applied, "a cancel must not apply to a run already closed");
    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.status, RunStatus::Complete, "the completion stands");
    assert_eq!(
        run.finished_at,
        Some(t(2)),
        "and it keeps the finish time the walk stamped"
    );
}

#[tokio::test]
async fn given_a_completed_run_when_a_cancel_with_a_tally_lands_afterwards_then_it_is_refused() {
    // The same guard on the other `cancel` branch -- the one a walk takes,
    // which carries its partial tally through `close_with_counts`. Without it
    // the guard would hold for the control handler and leak for the walk.
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Index, Some("/library"), t(1))
        .await
        .expect("start");
    repo.finish(
        id,
        RunCounts::Index {
            scanned: 13,
            indexed: 13,
            skipped: 0,
            already_cataloged: 0,
            failed: 0,
        },
        t(2),
    )
    .await
    .expect("finish");

    let applied = repo
        .cancel(
            id,
            Some(RunCounts::Index {
                scanned: 13,
                indexed: 4,
                skipped: 0,
                already_cataloged: 0,
                failed: 0,
            }),
            t(3),
        )
        .await
        .expect("cancel");

    assert!(!applied);
    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.status, RunStatus::Complete);
    assert_eq!(
        run.counts,
        Some(RunCounts::Index {
            scanned: 13,
            indexed: 13,
            skipped: 0,
            already_cataloged: 0,
            failed: 0
        }),
        "the completed tally must not be overwritten by the partial one"
    );
}

#[tokio::test]
async fn given_a_paused_run_when_cancelled_then_the_write_applies() {
    // Why the cancel guard is a set rather than a single value: abandoning a
    // paused run is the whole point of cancelling one, and a guard bound to
    // `running` alone would make a paused run impossible to be rid of.
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Index, Some("/library"), t(1))
        .await
        .expect("start");
    assert!(repo.pause(id, t(2)).await.expect("pause"));

    let applied = repo.cancel(id, None, t(3)).await.expect("cancel");

    assert!(applied);
    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.status, RunStatus::Cancelled);
    assert_eq!(run.finished_at, Some(t(3)));
}
