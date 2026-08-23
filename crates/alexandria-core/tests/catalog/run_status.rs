//! Unit tests for the UC-42 GetRunStatusHandler against trait fakes — no
//! database. Coverage: the main flow plus AF-01 (unknown id) and AF-02
//! (unauthenticated), AF-03's "running runs carry no counts", and FR-FC-28's
//! live progress overlay.

use chrono::{TimeZone, Utc};
use uuid::Uuid;

use alexandria_core::catalog::clock::FixedClock;
use alexandria_core::catalog::queries::active_runs::GetActiveRunsHandler;
use alexandria_core::catalog::queries::run_status::GetRunStatusHandler;
use alexandria_core::catalog::run_registry::{RunPhase, RunProgress, RunRegistry};
use alexandria_core::catalog::runs::{CatalogRunRepository, RunCounts, RunKind, RunStatus};
use alexandria_core::errors::DomainError;

use crate::common::{FakeAuth, FakeCatalogRunRepository};

const TOKEN: &str = "owner-token";

fn t(hour: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, hour, 0, 0).unwrap()
}

/// A clock reading `hour:minute:second` on the same day [`t`] uses, so a test
/// can assert an exact `active_millis` rather than a range.
fn at(hour: u32, minute: u32, second: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, hour, minute, second)
        .unwrap()
}

#[tokio::test]
async fn given_a_completed_run_when_read_then_it_is_returned_with_its_counts() {
    let runs = FakeCatalogRunRepository::new();
    let id = Uuid::new_v4();
    runs.start(id, RunKind::Refresh, None, t(1), 4)
        .await
        .unwrap();
    runs.finish(
        id,
        RunCounts::Refresh {
            refreshed: 2,
            marked_missing: 1,
            unchanged: 4,
            failed: 0,
        },
        t(2),
    )
    .await
    .unwrap();
    let handler = GetRunStatusHandler::new(
        FakeAuth::Allowing,
        runs,
        FixedClock(t(3)),
        RunRegistry::new(),
    );

    let run = handler.get(id, TOKEN).await.expect("get");

    assert_eq!(run.id, id);
    assert_eq!(run.kind, RunKind::Refresh);
    assert_eq!(run.status, RunStatus::Complete);
    assert_eq!(
        run.counts,
        Some(RunCounts::Refresh {
            refreshed: 2,
            marked_missing: 1,
            unchanged: 4,
            failed: 0
        })
    );
}

#[tokio::test]
async fn given_a_running_run_when_read_then_it_has_no_counts_yet() {
    // AF-03: no tally exists until the walk finishes.
    let runs = FakeCatalogRunRepository::new();
    let id = Uuid::new_v4();
    runs.start(id, RunKind::Index, Some("/library"), t(1), 4)
        .await
        .unwrap();
    let handler = GetRunStatusHandler::new(
        FakeAuth::Allowing,
        runs,
        FixedClock(t(3)),
        RunRegistry::new(),
    );

    let run = handler.get(id, TOKEN).await.expect("get");

    assert_eq!(run.status, RunStatus::Running);
    assert!(run.counts.is_none());
    assert!(run.finished_at.is_none());
}

#[tokio::test]
async fn given_an_unknown_run_id_when_read_then_not_found() {
    // AF-01.
    let handler = GetRunStatusHandler::new(
        FakeAuth::Allowing,
        FakeCatalogRunRepository::new(),
        FixedClock(t(3)),
        RunRegistry::new(),
    );

    let err = handler
        .get(Uuid::new_v4(), TOKEN)
        .await
        .expect_err("must reject an unknown id");

    assert!(matches!(err, DomainError::NotFound), "got {err:?}");
}

#[tokio::test]
async fn given_an_unauthenticated_caller_when_read_then_unauthorized() {
    // AF-02.
    let runs = FakeCatalogRunRepository::new();
    let id = Uuid::new_v4();
    runs.start(id, RunKind::Refresh, None, t(1), 4)
        .await
        .unwrap();
    let handler = GetRunStatusHandler::new(
        FakeAuth::Denying,
        runs,
        FixedClock(t(3)),
        RunRegistry::new(),
    );

    let err = handler
        .get(id, "")
        .await
        .expect_err("must reject an unauthenticated caller");

    assert!(matches!(err, DomainError::Unauthorized), "got {err:?}");
}

#[tokio::test]
async fn given_an_unauthenticated_caller_and_an_unknown_run_id_when_read_then_unauthorized_not_not_found(
) {
    // AF-02: a caller who never authenticated must not learn whether the id
    // names a run at all — auth is checked before the repository, so an
    // unknown id behind a denied token must still surface as Unauthorized,
    // never NotFound.
    let handler = GetRunStatusHandler::new(
        FakeAuth::Denying,
        FakeCatalogRunRepository::new(),
        FixedClock(t(3)),
        RunRegistry::new(),
    );

    let err = handler
        .get(Uuid::new_v4(), "")
        .await
        .expect_err("must reject an unauthenticated caller");

    assert!(matches!(err, DomainError::Unauthorized), "got {err:?}");
    assert!(
        !matches!(err, DomainError::NotFound),
        "must not leak whether the id names a run to an unauthenticated caller"
    );
}

#[tokio::test]
async fn given_a_running_run_with_a_live_cell_when_read_then_it_reports_live_progress() {
    // FR-FC-28: the live cell is authoritative while the run executes. The
    // persisted row deliberately carries a *stale* tally here — an earlier
    // flush — so a pass proves the overlay read the cell rather than the row.
    let runs = FakeCatalogRunRepository::new();
    let id = Uuid::new_v4();
    runs.start(id, RunKind::Index, Some("/library"), t(1), 4)
        .await
        .unwrap();
    runs.record_progress(
        id,
        &RunProgress {
            phase: RunPhase::Processing,
            total: Some(12_264),
            processed: 4_000,
        },
    )
    .await
    .unwrap();

    let registry = RunRegistry::new();
    let cell = registry.open(id);
    cell.set_phase(RunPhase::Processing);
    cell.set_total(12_264);
    for _ in 0..8_412 {
        cell.advance();
    }
    let handler = GetRunStatusHandler::new(FakeAuth::Allowing, runs, FixedClock(t(3)), registry);

    let run = handler.get(id, TOKEN).await.expect("get");

    assert_eq!(run.phase, Some(RunPhase::Processing));
    assert_eq!(run.total, Some(12_264));
    assert_eq!(
        run.processed,
        Some(8_412),
        "the live cell must win over the last flush"
    );
}

#[tokio::test]
async fn given_a_discovering_run_with_a_live_cell_when_read_then_the_total_is_still_unknown() {
    // Discovery has not counted the root yet, so there is no denominator to
    // publish — the client is told "unknown", not zero.
    let runs = FakeCatalogRunRepository::new();
    let id = Uuid::new_v4();
    runs.start(id, RunKind::Index, Some("/library"), t(1), 4)
        .await
        .unwrap();
    let registry = RunRegistry::new();
    // Bound, not dropped: the guard is what keeps the run open.
    let _cell = registry.open(id);
    let handler = GetRunStatusHandler::new(FakeAuth::Allowing, runs, FixedClock(t(3)), registry);

    let run = handler.get(id, TOKEN).await.expect("get");

    assert_eq!(run.phase, Some(RunPhase::Discovering));
    assert_eq!(run.total, None);
    assert_eq!(run.processed, Some(0));
}

#[tokio::test]
async fn given_a_run_with_no_live_cell_when_read_then_it_reports_the_persisted_progress() {
    // The process restarted, or the run stopped: nothing is executing under
    // this id, so the last flush is the best answer there is.
    let runs = FakeCatalogRunRepository::new();
    let id = Uuid::new_v4();
    runs.start(id, RunKind::Index, Some("/library"), t(1), 4)
        .await
        .unwrap();
    runs.record_progress(
        id,
        &RunProgress {
            phase: RunPhase::Processing,
            total: Some(12_264),
            processed: 8_412,
        },
    )
    .await
    .unwrap();
    let handler = GetRunStatusHandler::new(
        FakeAuth::Allowing,
        runs,
        FixedClock(t(3)),
        RunRegistry::new(),
    );

    let run = handler.get(id, TOKEN).await.expect("get");

    assert_eq!(run.phase, Some(RunPhase::Processing));
    assert_eq!(run.total, Some(12_264));
    assert_eq!(
        run.processed,
        Some(8_412),
        "a restart falls back to the last flush"
    );
}

#[tokio::test]
async fn given_a_run_that_never_flushed_when_read_then_it_reports_no_progress() {
    // A run that stopped inside discovery has no flush behind it. That is
    // "unknown", not zero-of-zero.
    let runs = FakeCatalogRunRepository::new();
    let id = Uuid::new_v4();
    runs.start(id, RunKind::Index, Some("/library"), t(1), 4)
        .await
        .unwrap();
    let handler = GetRunStatusHandler::new(
        FakeAuth::Allowing,
        runs,
        FixedClock(t(3)),
        RunRegistry::new(),
    );

    let run = handler.get(id, TOKEN).await.expect("get");

    assert_eq!(run.phase, None);
    assert_eq!(run.total, None);
    assert_eq!(run.processed, None);
}

#[tokio::test]
async fn given_a_running_run_when_read_then_active_millis_counts_up_to_now() {
    // A running run has no `finished_at`, so the clock stands in for it —
    // which is why the handler holds one.
    let runs = FakeCatalogRunRepository::new();
    let id = Uuid::new_v4();
    runs.start(id, RunKind::Index, Some("/library"), at(1, 0, 0), 4)
        .await
        .unwrap();
    let handler = GetRunStatusHandler::new(
        FakeAuth::Allowing,
        runs,
        FixedClock(at(1, 0, 30)),
        RunRegistry::new(),
    );

    let run = handler.get(id, TOKEN).await.expect("get");

    assert_eq!(run.active_millis, 30_000);
}

#[tokio::test]
async fn given_a_finished_run_when_read_then_active_millis_stops_at_finished_at() {
    let runs = FakeCatalogRunRepository::new();
    let id = Uuid::new_v4();
    runs.start(id, RunKind::Refresh, None, at(1, 0, 0), 4)
        .await
        .unwrap();
    runs.finish(
        id,
        RunCounts::Refresh {
            refreshed: 1,
            marked_missing: 0,
            unchanged: 0,
            failed: 0,
        },
        at(1, 0, 10),
    )
    .await
    .unwrap();
    let handler = GetRunStatusHandler::new(
        FakeAuth::Allowing,
        runs,
        // Far past the finish: a finished run's elapsed time must not keep
        // growing with the wall clock.
        FixedClock(at(5, 0, 0)),
        RunRegistry::new(),
    );

    let run = handler.get(id, TOKEN).await.expect("get");

    assert_eq!(run.active_millis, 10_000);
}

#[tokio::test]
async fn given_a_run_that_spent_time_paused_when_read_then_active_millis_excludes_it() {
    // `active_millis` is time the run was *working*: wall time minus the time
    // it spent paused. Task 8's pause/resume is what populates
    // `paused_millis`; this asserts the arithmetic it feeds.
    let runs = FakeCatalogRunRepository::new();
    let id = Uuid::new_v4();
    runs.start(id, RunKind::Index, Some("/library"), at(1, 0, 0), 4)
        .await
        .unwrap();
    runs.set_paused_millis(id, 20_000);
    let handler = GetRunStatusHandler::new(
        FakeAuth::Allowing,
        runs,
        FixedClock(at(1, 0, 30)),
        RunRegistry::new(),
    );

    let run = handler.get(id, TOKEN).await.expect("get");

    assert_eq!(run.active_millis, 10_000);
    assert_eq!(run.paused_millis, 20_000);
}

#[tokio::test]
async fn given_a_paused_run_when_read_later_then_active_millis_does_not_grow_with_the_pause() {
    // `paused_millis` only holds pauses that have *ended* — a resume banks
    // them — so the stretch a run is sitting in right now has to be
    // subtracted separately or it is counted as work. This is not a corner
    // case: startup reconciliation pauses a run left over from a previous
    // launch and writes no `finished_at`, so without this the run's clock
    // runs for every day the application stays shut, and a client dividing
    // `processed` by `active_millis` to estimate what is left gets an answer
    // that degrades the longer the owner leaves the run alone.
    let runs = FakeCatalogRunRepository::new();
    let id = Uuid::new_v4();
    runs.start(id, RunKind::Index, Some("/library"), at(1, 0, 0), 4)
        .await
        .unwrap();
    assert!(runs.pause(id, at(1, 10, 0), None).await.expect("pause"));

    // Ten minutes of work, then a pause of no particular length. Every read
    // must give the same answer, whenever it happens.
    for read_at in [at(1, 10, 0), at(1, 40, 0), at(5, 0, 0)] {
        let handler = GetRunStatusHandler::new(
            FakeAuth::Allowing,
            runs.clone(),
            FixedClock(read_at),
            RunRegistry::new(),
        );

        let run = handler.get(id, TOKEN).await.expect("get");

        assert_eq!(
            run.active_millis,
            10 * 60 * 1000,
            "a run reads the same at {read_at} as it did the moment it paused"
        );
    }
}

#[tokio::test]
async fn given_a_run_cancelled_while_paused_when_read_then_its_clock_is_frozen() {
    // Cancel does not clear `paused_at`, so the open pause above has to be
    // measured to the finish time rather than to now — otherwise a terminal
    // run's elapsed time would start moving again, in the wrong direction.
    let runs = FakeCatalogRunRepository::new();
    let id = Uuid::new_v4();
    runs.start(id, RunKind::Index, Some("/library"), at(1, 0, 0), 4)
        .await
        .unwrap();
    assert!(runs.pause(id, at(1, 10, 0), None).await.expect("pause"));
    assert!(runs.cancel(id, None, at(1, 20, 0)).await.expect("cancel"));
    let handler = GetRunStatusHandler::new(
        FakeAuth::Allowing,
        runs.clone(),
        FixedClock(at(9, 0, 0)),
        RunRegistry::new(),
    );

    let run = handler.get(id, TOKEN).await.expect("get");

    assert_eq!(
        run.active_millis,
        10 * 60 * 1000,
        "twenty minutes elapsed to the cancel, of which the last ten were paused"
    );
}

// ---------------- GetActiveRunsHandler (Task 10) ----------------

/// The handler under test plus the repository and registry a test needs to
/// seed runs into. Mirrors `run_control.rs`'s `ControlHarness`.
struct ActiveRunsHarness {
    handler: GetActiveRunsHandler<FakeAuth, FakeCatalogRunRepository, FixedClock>,
    runs: FakeCatalogRunRepository,
    registry: RunRegistry,
}

impl ActiveRunsHarness {
    async fn new() -> Self {
        let runs = FakeCatalogRunRepository::new();
        let registry = RunRegistry::new();
        Self {
            handler: GetActiveRunsHandler::new(
                FakeAuth::Allowing,
                runs.clone(),
                FixedClock(t(9)),
                registry.clone(),
            ),
            runs,
            registry,
        }
    }

    /// Start a run and, unless it is `Running`, drive it into `status`
    /// through the repository's own transitions — the same reasoning as
    /// `ControlHarness::with_run`: seeding through the real transitions keeps
    /// the fake and the adapter from drifting on what each status leaves
    /// behind.
    async fn a_run(&self, status: RunStatus) -> Uuid {
        let id = Uuid::new_v4();
        self.runs
            .start(id, RunKind::Index, Some("/library"), t(1), 4)
            .await
            .unwrap();
        match status {
            RunStatus::Running => {}
            RunStatus::Paused => {
                assert!(self.runs.pause(id, t(2), None).await.unwrap());
            }
            RunStatus::Complete => self
                .runs
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
                .await
                .unwrap(),
            RunStatus::Failed => self.runs.fail(id, "root unreadable", t(2)).await.unwrap(),
            RunStatus::Cancelled => {
                assert!(self.runs.cancel(id, None, t(2)).await.unwrap());
            }
        }
        id
    }
}

#[tokio::test]
async fn given_runs_in_every_state_when_active_ones_are_listed_then_only_running_and_paused_are_returned(
) {
    let harness = ActiveRunsHarness::new().await;
    let running = harness.a_run(RunStatus::Running).await;
    let paused = harness.a_run(RunStatus::Paused).await;
    harness.a_run(RunStatus::Complete).await;
    harness.a_run(RunStatus::Failed).await;
    harness.a_run(RunStatus::Cancelled).await;

    let active = harness.handler.list("token").await.unwrap();

    let ids: Vec<_> = active.iter().map(|r| r.id).collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&running) && ids.contains(&paused));
}

#[tokio::test]
async fn given_no_outstanding_runs_when_listed_then_an_empty_list_is_returned_not_an_error() {
    // An idle library is the normal case, not an error condition.
    let harness = ActiveRunsHarness::new().await;
    harness.a_run(RunStatus::Complete).await;

    let active = harness.handler.list("token").await.expect("must not error");

    assert!(active.is_empty());
}

#[tokio::test]
async fn given_an_unauthenticated_caller_when_active_runs_are_listed_then_unauthorized() {
    let runs = FakeCatalogRunRepository::new();
    let handler = GetActiveRunsHandler::new(
        FakeAuth::Denying,
        runs,
        FixedClock(t(9)),
        RunRegistry::new(),
    );

    let err = handler
        .list("")
        .await
        .expect_err("must reject an unauthenticated caller");

    assert!(matches!(err, DomainError::Unauthorized), "got {err:?}");
}

#[tokio::test]
async fn given_a_live_run_in_the_list_when_read_then_it_reports_live_progress_not_the_last_flush() {
    // The same overlay `GetRunStatusHandler` performs: a client listing
    // outstanding runs wants current numbers, not the last flush. The
    // persisted row deliberately carries a *stale* tally so a pass proves the
    // overlay read the cell rather than the row.
    let harness = ActiveRunsHarness::new().await;
    let id = harness.a_run(RunStatus::Running).await;
    harness
        .runs
        .record_progress(
            id,
            &RunProgress {
                phase: RunPhase::Processing,
                total: Some(12_264),
                processed: 4_000,
            },
        )
        .await
        .unwrap();

    let cell = harness.registry.open(id);
    cell.set_phase(RunPhase::Processing);
    cell.set_total(12_264);
    for _ in 0..8_412 {
        cell.advance();
    }

    let active = harness.handler.list("token").await.unwrap();

    let run = active.iter().find(|r| r.id == id).expect("run in list");
    assert_eq!(run.phase, Some(RunPhase::Processing));
    assert_eq!(run.total, Some(12_264));
    assert_eq!(
        run.processed,
        Some(8_412),
        "the live cell must win over the last flush"
    );
}

#[tokio::test]
async fn given_active_runs_when_listed_then_they_come_back_newest_first() {
    let harness = ActiveRunsHarness::new().await;
    let older = Uuid::new_v4();
    harness
        .runs
        .start(older, RunKind::Index, Some("/library"), t(1), 4)
        .await
        .unwrap();
    let newer = Uuid::new_v4();
    harness
        .runs
        .start(newer, RunKind::Index, Some("/library"), t(5), 4)
        .await
        .unwrap();

    let active = harness.handler.list("token").await.unwrap();

    let ids: Vec<_> = active.iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![newer, older], "newest started run first");
}
