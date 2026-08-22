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

/// As [`repo`], but keeping the pool as well - for the pause columns, which
/// real SQL reads back but no command writes yet (pause/resume is a later
/// task), so the only way to exercise that read is to seed the row directly.
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
async fn given_running_and_terminal_runs_when_reconciled_then_only_running_becomes_interrupted() {
    // FR-FC-29: runs execute in-process and are never resumed, so a row still
    // `running` at startup provably has no task behind it. Terminal rows must
    // be left exactly as they are.
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

    let reconciled = repo.interrupt_running(t(3)).await.expect("interrupt");

    assert_eq!(reconciled, 1, "only the running row is reconciled");
    let run = repo.get(running).await.unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Interrupted);
    assert_eq!(run.finished_at, Some(t(3)));
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
    assert_eq!(repo.interrupt_running(t(3)).await.expect("interrupt"), 0);
}

#[tokio::test]
async fn given_a_run_left_running_when_services_are_built_then_it_is_interrupted() {
    // FR-FC-29 end to end: a run recorded as running by a previous process is
    // reconciled at startup, so a client polling it gets a terminal answer
    // instead of waiting on a run that cannot finish.
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
    assert_eq!(run.status, RunStatus::Interrupted);
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
async fn given_an_interrupted_run_when_read_then_it_publishes_no_phase() {
    // Startup reconciliation (FR-FC-29) makes a run terminal too, so it has
    // to clear the phase for the same reason `finish` and `fail` do.
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

    repo.interrupt_running(t(2)).await.expect("interrupt");

    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.status, RunStatus::Interrupted);
    assert_eq!(run.phase, None);
    assert_eq!(
        run.processed,
        Some(5),
        "the last flush is what an interrupted run has to show for itself"
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

    repo.pause(id, t(2)).await.expect("pause");

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

    repo.cancel(id, t(2)).await.expect("cancel");

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
    repo.pause(id, t(2)).await.expect("pause");

    let reconciled = repo.interrupt_running(t(3)).await.expect("interrupt");

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
