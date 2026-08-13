//! Tests for the UC-42 run record: the SQLite repository against a real
//! migrated database (Testing Specification §6.4), covering each lifecycle
//! transition and the startup reconciliation FR-FC-29 requires.

use chrono::{TimeZone, Utc};
use uuid::Uuid;

use alexandria_core::catalog::runs::{
    CatalogRunRepository, RunCounts, RunKind, RunStatus, SqliteCatalogRunRepository,
};
use alexandria_core::migrate::migrate_database;

async fn repo() -> (SqliteCatalogRunRepository, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("alexandria.sqlite");
    let pool = migrate_database(path.to_str().expect("path"))
        .await
        .expect("migrate");
    (SqliteCatalogRunRepository::new(pool), dir)
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
            scanned: 10,
            indexed: 7,
            skipped: 2,
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
            scanned: 10,
            indexed: 7,
            skipped: 2,
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
    assert!(value.get("root").is_none(), "a refresh has no root");
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
    assert!(
        value.get("failed").is_none(),
        "no failed-count key on a run with no tally"
    );
}
