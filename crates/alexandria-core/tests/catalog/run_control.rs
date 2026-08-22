//! Unit tests for `RunControlHandler` against trait fakes — no database.
//!
//! Coverage: authentication ahead of everything else, the unknown id, every
//! legal transition, every illegal one, and the two ways a legal call is
//! carried out — raising the signal on a live cell, or writing the row
//! directly for a run this process is not executing.

use chrono::{TimeZone, Utc};
use uuid::Uuid;

use alexandria_core::catalog::clock::FixedClock;
use alexandria_core::catalog::commands::run_control::RunControlHandler;
use alexandria_core::catalog::run_registry::{RunCellGuard, RunRegistry, RunSignal};
use alexandria_core::catalog::runs::{CatalogRunRepository, RunCounts, RunKind, RunStatus};
use alexandria_core::errors::DomainError;

use crate::common::{FakeAuth, FakeCatalogRunRepository};

const TOKEN: &str = "owner-token";

fn t(hour: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, hour, 0, 0).unwrap()
}

/// The handler under test plus the two things a test needs to see what it
/// did: the repository row it may have written, and the registry cell it may
/// have signalled.
struct ControlHarness {
    control: RunControlHandler<FakeAuth, FakeCatalogRunRepository, FixedClock>,
    runs: FakeCatalogRunRepository,
    registry: RunRegistry,
    run_id: Uuid,
    /// Held for the lifetime of the harness when the run is live — dropping
    /// it would close the cell, which is exactly what "no live cell" means.
    _cell: Option<RunCellGuard>,
}

impl ControlHarness {
    /// A harness whose run has been driven into `status` and whose process is
    /// gone — no live cell. The seeding goes through the repository's own
    /// transitions rather than poking a field, so the fake and the real
    /// adapter cannot drift on what each status leaves behind.
    async fn with_run(status: RunStatus) -> Self {
        let runs = FakeCatalogRunRepository::new();
        let run_id = Uuid::new_v4();
        runs.start(run_id, RunKind::Index, Some("/library"), t(1))
            .await
            .expect("start");
        match status {
            RunStatus::Running => {}
            RunStatus::Complete => runs
                .finish(
                    run_id,
                    RunCounts::Index {
                        scanned: 3,
                        indexed: 3,
                        skipped: 0,
                        already_cataloged: 0,
                        failed: 0,
                    },
                    t(2),
                )
                .await
                .expect("finish"),
            RunStatus::Failed => runs
                .fail(run_id, "root unreadable", t(2))
                .await
                .expect("fail"),
            RunStatus::Interrupted => {
                runs.interrupt_running(t(2)).await.expect("interrupt");
            }
            RunStatus::Paused => {
                assert!(runs.pause(run_id, t(2)).await.expect("pause"));
            }
            RunStatus::Cancelled => runs.cancel(run_id, None, t(2)).await.expect("cancel"),
        }
        let registry = RunRegistry::new();
        Self {
            control: RunControlHandler::new(
                FakeAuth::Allowing,
                runs.clone(),
                FixedClock(t(3)),
                registry.clone(),
            ),
            runs,
            registry,
            run_id,
            _cell: None,
        }
    }

    /// A running run this process *is* executing: same as
    /// [`ControlHarness::with_run`] with a `Running` row, plus the open cell
    /// its processing loop would be holding.
    async fn with_live_running_run() -> Self {
        let mut harness = Self::with_run(RunStatus::Running).await;
        harness._cell = Some(harness.registry.open(harness.run_id));
        harness
    }

    fn recorded_status(&self) -> RunStatus {
        self.runs
            .get_recorded(self.run_id)
            .expect("recorded run")
            .status
    }

    fn raised_signal(&self) -> RunSignal {
        self.registry
            .get(self.run_id)
            .expect("a live run has a cell")
            .signal()
    }
}

/// Every status a control call must refuse to pause.
const UNPAUSABLE: [RunStatus; 5] = [
    RunStatus::Paused,
    RunStatus::Complete,
    RunStatus::Failed,
    RunStatus::Interrupted,
    RunStatus::Cancelled,
];

/// Every status a control call must refuse to cancel. `Paused` is absent
/// deliberately — abandoning a paused run is the whole point of cancelling
/// one, and without it a paused run could never be got rid of.
const UNCANCELLABLE: [RunStatus; 4] = [
    RunStatus::Complete,
    RunStatus::Failed,
    RunStatus::Interrupted,
    RunStatus::Cancelled,
];

#[tokio::test]
async fn given_a_live_running_run_when_paused_then_the_signal_is_raised_and_the_row_is_left_alone()
{
    // The loop owns the row for a run it is executing: it writes the pause
    // once its in-flight window has drained and its final tally is flushed.
    // Writing the row from here too would race that write.
    let harness = ControlHarness::with_live_running_run().await;

    harness.control.pause(harness.run_id, TOKEN).await.unwrap();

    assert_eq!(harness.raised_signal(), RunSignal::Pause);
    assert_eq!(
        harness.recorded_status(),
        RunStatus::Running,
        "the run's own loop writes the paused row, not the control call"
    );
}

#[tokio::test]
async fn given_a_live_running_run_when_cancelled_then_the_signal_is_raised_and_the_row_is_left_alone(
) {
    let harness = ControlHarness::with_live_running_run().await;

    harness.control.cancel(harness.run_id, TOKEN).await.unwrap();

    assert_eq!(harness.raised_signal(), RunSignal::Cancel);
    assert_eq!(harness.recorded_status(), RunStatus::Running);
}

#[tokio::test]
async fn given_a_running_run_with_no_live_cell_when_paused_then_it_is_recorded_paused() {
    // A `running` row with no cell is a run whose process is gone. Pausing it
    // is the least destructive answer available: it stops nothing (there is
    // nothing left to stop) but leaves the run resumable, where cancelling
    // would throw that away and refusing would leave the row `running`
    // forever.
    let harness = ControlHarness::with_run(RunStatus::Running).await;

    harness.control.pause(harness.run_id, TOKEN).await.unwrap();

    let run = harness.runs.get_recorded(harness.run_id).expect("run");
    assert_eq!(run.status, RunStatus::Paused);
    assert_eq!(
        run.paused_at,
        Some(t(3)),
        "stamped from the handler's clock"
    );
    assert!(
        run.finished_at.is_none(),
        "a paused run has not finished — it can still be resumed"
    );
}

#[tokio::test]
async fn given_a_running_run_with_no_live_cell_when_cancelled_then_it_is_recorded_cancelled() {
    let harness = ControlHarness::with_run(RunStatus::Running).await;

    harness.control.cancel(harness.run_id, TOKEN).await.unwrap();

    let run = harness.runs.get_recorded(harness.run_id).expect("run");
    assert_eq!(run.status, RunStatus::Cancelled);
    assert_eq!(run.finished_at, Some(t(3)), "cancel is terminal");
}

#[tokio::test]
async fn given_a_paused_run_when_cancelled_then_it_is_recorded_cancelled() {
    // A paused run has no loop left to signal, so the control call writes the
    // row itself. Without this a paused run could never be abandoned.
    let harness = ControlHarness::with_run(RunStatus::Paused).await;

    harness.control.cancel(harness.run_id, TOKEN).await.unwrap();

    assert_eq!(harness.recorded_status(), RunStatus::Cancelled);
}

#[tokio::test]
async fn given_a_paused_run_when_paused_again_then_invalid_state() {
    let harness = ControlHarness::with_run(RunStatus::Paused).await;

    let result = harness.control.pause(harness.run_id, TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidState)));
}

#[tokio::test]
async fn given_a_completed_run_when_cancelled_then_invalid_state() {
    let harness = ControlHarness::with_run(RunStatus::Complete).await;

    let result = harness.control.cancel(harness.run_id, TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidState)));
}

#[tokio::test]
async fn given_an_unpausable_run_when_paused_then_invalid_state_and_the_row_is_untouched() {
    for status in UNPAUSABLE {
        let harness = ControlHarness::with_run(status).await;

        let result = harness.control.pause(harness.run_id, TOKEN).await;

        assert!(
            matches!(result, Err(DomainError::InvalidState)),
            "pausing a {status:?} run must be refused, got {result:?}"
        );
        assert_eq!(
            harness.recorded_status(),
            status,
            "a refused transition writes nothing"
        );
    }
}

#[tokio::test]
async fn given_an_uncancellable_run_when_cancelled_then_invalid_state_and_the_row_is_untouched() {
    for status in UNCANCELLABLE {
        let harness = ControlHarness::with_run(status).await;

        let result = harness.control.cancel(harness.run_id, TOKEN).await;

        assert!(
            matches!(result, Err(DomainError::InvalidState)),
            "cancelling a {status:?} run must be refused, got {result:?}"
        );
        assert_eq!(
            harness.recorded_status(),
            status,
            "a refused transition writes nothing"
        );
    }
}

#[tokio::test]
async fn given_an_unknown_run_when_paused_then_not_found() {
    let harness = ControlHarness::with_run(RunStatus::Running).await;

    let result = harness.control.pause(Uuid::new_v4(), TOKEN).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

#[tokio::test]
async fn given_an_unknown_run_when_cancelled_then_not_found() {
    let harness = ControlHarness::with_run(RunStatus::Running).await;

    let result = harness.control.cancel(Uuid::new_v4(), TOKEN).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

#[tokio::test]
async fn given_an_unauthenticated_caller_when_pausing_then_unauthorized_not_invalid_state() {
    // Authentication runs before the state machine, so a bad token learns
    // nothing about the run — not whether it exists, not what state it is in.
    let mut harness = ControlHarness::with_run(RunStatus::Complete).await;
    harness.control = RunControlHandler::new(
        FakeAuth::Denying,
        harness.runs.clone(),
        FixedClock(t(3)),
        harness.registry.clone(),
    );

    let result = harness.control.pause(harness.run_id, "bad-token").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

#[tokio::test]
async fn given_an_unauthenticated_caller_when_cancelling_an_unknown_run_then_unauthorized_not_found(
) {
    // The same ordering seen from the other side: a bad token must not be
    // able to probe which run ids exist.
    let harness = ControlHarness::with_run(RunStatus::Running).await;
    let control = RunControlHandler::new(
        FakeAuth::Denying,
        harness.runs.clone(),
        FixedClock(t(3)),
        harness.registry.clone(),
    );

    let result = control.cancel(Uuid::new_v4(), "bad-token").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

#[tokio::test]
async fn given_a_cancelled_run_when_a_pause_write_lands_afterwards_then_the_fake_refuses_it_too() {
    // The fake must mirror the SQLite adapter's `AND status = 'running'`
    // guard, or every handler test above would be passing against a
    // repository more permissive than the real one — and the race the guard
    // exists for would be invisible here.
    let runs = FakeCatalogRunRepository::new();
    let run_id = Uuid::new_v4();
    runs.start(run_id, RunKind::Index, Some("/library"), t(1))
        .await
        .expect("start");
    runs.cancel(run_id, None, t(2)).await.expect("cancel");

    let applied = runs.pause(run_id, t(3)).await.expect("pause");

    assert!(!applied, "a pause must not apply to a run already closed");
    let run = runs.get_recorded(run_id).expect("run");
    assert_eq!(run.status, RunStatus::Cancelled, "the cancel stands");
    assert_eq!(run.finished_at, Some(t(2)));
    assert!(run.paused_at.is_none());
}

#[tokio::test]
async fn given_a_run_closed_between_the_lookup_and_the_write_when_paused_then_invalid_state() {
    // The handler's own half of the race guard, and the only test that
    // reaches it: the run reads `running` at the lookup, and is closed by
    // someone else before this call's write lands. Closing it *before* the
    // call would prove something different — the state machine's rejection
    // rather than the write's — so the fake is armed to move the row on
    // immediately after it answers the lookup.
    let harness = ControlHarness::with_run(RunStatus::Running).await;
    harness.runs.cancel_after_next_get(harness.run_id);

    let result = harness.control.pause(harness.run_id, TOKEN).await;

    assert!(
        matches!(result, Err(DomainError::InvalidState)),
        "a pause whose write is refused must be reported, not silently swallowed; got {result:?}"
    );
    assert_eq!(
        harness.recorded_status(),
        RunStatus::Cancelled,
        "the cancel stands — the late pause must not have overwritten it"
    );
    let run = harness.runs.get_recorded(harness.run_id).expect("run");
    assert!(
        run.paused_at.is_none(),
        "and it must not have stamped a pause time on a cancelled run"
    );
}
