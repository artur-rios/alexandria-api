//! Unit tests for `RunControlHandler` against trait fakes — no database.
//!
//! Coverage: authentication ahead of everything else, the unknown id, every
//! legal transition, every illegal one, and the two ways a legal call is
//! carried out — raising the signal on a live cell, or writing the row
//! directly for a run this process is not executing. Resume is here too: it
//! is the third verb of the same state machine, and the only one that both
//! writes the row and answers with something.

use chrono::{TimeZone, Utc};
use uuid::Uuid;

use alexandria_core::catalog::clock::FixedClock;
use alexandria_core::catalog::commands::run_control::RunControlHandler;
use alexandria_core::catalog::queries::run_status::GetRunStatusHandler;
use alexandria_core::catalog::run_registry::{
    RunCellGuard, RunPhase, RunProgress, RunRegistry, RunSignal,
};
use alexandria_core::catalog::runs::{
    CatalogRunRepository, RunCounts, RunKind, RunPriority, RunStatus,
};
use alexandria_core::errors::DomainError;

use crate::common::{FakeAuth, FakeCatalogRunRepository};

const TOKEN: &str = "owner-token";

/// What the handler resumes a run at when the row itself records no width —
/// `RunControlHandler`'s configured fallback. Also the width these tests seed
/// `start` with, since none of them (other than the one that explicitly
/// clears it via `FakeCatalogRunRepository::clear_concurrency`) cares whether
/// the row's own value or the fallback answered.
const DEFAULT_CONCURRENCY: u32 = 4;

/// What `RunPriority::Low` resolves to here
/// (`indexing.low_priority_concurrency`). Deliberately distinct from
/// [`DEFAULT_CONCURRENCY`], so a resume that names a priority can be shown to
/// have picked the *right* one of the two rather than either one.
const LOW_PRIORITY_CONCURRENCY: u32 = 2;

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
        runs.start(
            run_id,
            RunKind::Index,
            Some("/library"),
            t(1),
            DEFAULT_CONCURRENCY,
        )
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
            RunStatus::Paused => {
                assert!(runs.pause(run_id, t(2)).await.expect("pause"));
            }
            RunStatus::Cancelled => {
                assert!(runs.cancel(run_id, None, t(2)).await.expect("cancel"));
            }
        }
        let registry = RunRegistry::new();
        Self {
            control: RunControlHandler::new(
                FakeAuth::Allowing,
                runs.clone(),
                FixedClock(t(3)),
                registry.clone(),
                DEFAULT_CONCURRENCY,
                LOW_PRIORITY_CONCURRENCY,
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
const UNPAUSABLE: [RunStatus; 4] = [
    RunStatus::Paused,
    RunStatus::Complete,
    RunStatus::Failed,
    RunStatus::Cancelled,
];

/// Every status a control call must refuse to cancel. `Paused` is absent
/// deliberately — abandoning a paused run is the whole point of cancelling
/// one, and without it a paused run could never be got rid of.
const UNCANCELLABLE: [RunStatus; 3] =
    [RunStatus::Complete, RunStatus::Failed, RunStatus::Cancelled];

/// Every status a resume must refuse. `Paused` is the only edge into
/// `running`: a run already running has nothing to resume, and the three
/// terminal statuses have no run left at all.
const UNRESUMABLE: [RunStatus; 4] = [
    RunStatus::Running,
    RunStatus::Complete,
    RunStatus::Failed,
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
        DEFAULT_CONCURRENCY,
        LOW_PRIORITY_CONCURRENCY,
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
        DEFAULT_CONCURRENCY,
        LOW_PRIORITY_CONCURRENCY,
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
    runs.start(
        run_id,
        RunKind::Index,
        Some("/library"),
        t(1),
        DEFAULT_CONCURRENCY,
    )
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

/// A time on the same day as [`t`], to the minute — the pause and resume
/// arithmetic below is about intervals shorter than an hour.
fn at(hour: u32, minute: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, hour, minute, 0).unwrap()
}

/// A run started at 01:00, walked as far as `processed` of 12,264 entries,
/// and paused at `paused_at`. Returned with a handler whose clock reads
/// `now`, so a test fixes both ends of the pause it is measuring.
async fn paused_run(
    paused_at: chrono::DateTime<Utc>,
    processed: usize,
    now: chrono::DateTime<Utc>,
) -> (
    RunControlHandler<FakeAuth, FakeCatalogRunRepository, FixedClock>,
    FakeCatalogRunRepository,
    Uuid,
) {
    let runs = FakeCatalogRunRepository::new();
    let run_id = Uuid::new_v4();
    runs.start(
        run_id,
        RunKind::Index,
        Some("D:/music"),
        t(1),
        DEFAULT_CONCURRENCY,
    )
    .await
    .expect("start");
    // Simulates a run started before run priority existed — `start` always
    // writes a width now, so the "resumes at the configured default" test
    // below has to unset it explicitly to still exercise that fallback.
    runs.clear_concurrency(run_id);
    runs.record_progress(
        run_id,
        &RunProgress {
            phase: RunPhase::Processing,
            total: Some(12_264),
            processed,
        },
    )
    .await
    .expect("record progress");
    assert!(runs.pause(run_id, paused_at).await.expect("pause"));
    let control = RunControlHandler::new(
        FakeAuth::Allowing,
        runs.clone(),
        FixedClock(now),
        RunRegistry::new(),
        DEFAULT_CONCURRENCY,
        LOW_PRIORITY_CONCURRENCY,
    );
    (control, runs, run_id)
}

#[tokio::test]
async fn given_a_paused_run_when_resumed_then_it_runs_again_from_zero_with_the_paused_time_banked()
{
    // The segment counters restart because resume re-walks: `processed` counts
    // what one segment folded, and is never a position to seek to. What is
    // *not* thrown away is the pause itself — banked into `paused_millis`, so
    // the half hour the owner was away is not later reported as work.
    let (control, runs, run_id) = paused_run(at(1, 10), 8_412, at(1, 40)).await;

    let resumed = control.resume(run_id, TOKEN, None).await.expect("resume");

    let run = runs.get_recorded(run_id).expect("recorded run");
    assert_eq!(run.status, RunStatus::Running);
    assert_eq!(run.paused_at, None);
    assert_eq!(
        run.paused_millis,
        30 * 60 * 1000,
        "paused time is banked, not counted as work"
    );
    assert_eq!(run.processed, Some(0), "the segment's counter restarts");
    assert_eq!(run.total, None, "and its denominator is rediscovered");
    assert_eq!(
        run.phase,
        Some(RunPhase::Discovering),
        "a resumed run starts where every run starts"
    );
    assert_eq!(resumed.run_id, run_id);
    assert_eq!(resumed.root, Some("D:/music".to_string()));
    assert_eq!(resumed.kind, RunKind::Index);
    assert_eq!(
        resumed.concurrency, DEFAULT_CONCURRENCY,
        "a run whose row records no width resumes at the configured one"
    );
}

/// A run started at `width`, paused, and handed back with a handler whose
/// clock has not moved — everything the re-pacing tests below need and
/// nothing they do not. Unlike [`paused_run`] the stored width is *kept*:
/// these tests are about what happens to it.
async fn paused_run_at_width(
    width: u32,
) -> (
    RunControlHandler<FakeAuth, FakeCatalogRunRepository, FixedClock>,
    FakeCatalogRunRepository,
    Uuid,
) {
    let runs = FakeCatalogRunRepository::new();
    let run_id = Uuid::new_v4();
    runs.start(run_id, RunKind::Index, Some("D:/music"), t(1), width)
        .await
        .expect("start");
    assert!(runs.pause(run_id, t(2)).await.expect("pause"));
    let control = RunControlHandler::new(
        FakeAuth::Allowing,
        runs.clone(),
        FixedClock(t(3)),
        RunRegistry::new(),
        DEFAULT_CONCURRENCY,
        LOW_PRIORITY_CONCURRENCY,
    );
    (control, runs, run_id)
}

/// The whole of Task 15 at handler level: a run started wide, resumed at
/// `Low`, must come back *narrow* — and the row has to say so, because
/// `execute` reads the width off the row (decision 9) and would otherwise
/// re-pace nothing.
///
/// The started width is [`DEFAULT_CONCURRENCY`] and the resumed one
/// [`LOW_PRIORITY_CONCURRENCY`]; they differ, so neither "ignored the
/// priority" nor "always answers `Low`" can pass.
#[tokio::test]
async fn given_a_paused_run_when_resumed_at_low_priority_then_the_stored_width_is_the_low_one() {
    let (control, runs, run_id) = paused_run_at_width(DEFAULT_CONCURRENCY).await;

    let resumed = control
        .resume(run_id, TOKEN, Some(RunPriority::Low))
        .await
        .expect("resume");

    assert_eq!(
        resumed.concurrency, LOW_PRIORITY_CONCURRENCY,
        "the caller is told to spawn the walk at the width it just asked for"
    );
    assert_eq!(
        runs.get_recorded(run_id).expect("run").concurrency,
        Some(LOW_PRIORITY_CONCURRENCY),
        "and the row records it, which is the only thing `execute` reads — a \
         resume that answered the new width without persisting it would re-pace \
         nothing at all"
    );
}

/// The other direction, which is not the same test: a run throttled down and
/// then let back up. `None` cannot stand in for this — that is what makes
/// `"normal"` a real request rather than a synonym for silence.
#[tokio::test]
async fn given_a_low_priority_run_when_resumed_at_normal_priority_then_the_stored_width_is_widened()
{
    let (control, runs, run_id) = paused_run_at_width(LOW_PRIORITY_CONCURRENCY).await;

    let resumed = control
        .resume(run_id, TOKEN, Some(RunPriority::Normal))
        .await
        .expect("resume");

    assert_eq!(resumed.concurrency, DEFAULT_CONCURRENCY);
    assert_eq!(
        runs.get_recorded(run_id).expect("run").concurrency,
        Some(DEFAULT_CONCURRENCY),
        "`normal` is a request to widen, not a no-op"
    );
}

/// The backward-compatible path, and the reason absent is not spelled
/// `Normal`: every caller written before resume took a priority passes
/// nothing, and a run they throttled down must stay throttled down.
#[tokio::test]
async fn given_a_low_priority_run_when_resumed_without_a_priority_then_it_keeps_its_stored_width() {
    let (control, runs, run_id) = paused_run_at_width(LOW_PRIORITY_CONCURRENCY).await;

    let resumed = control.resume(run_id, TOKEN, None).await.expect("resume");

    assert_eq!(
        resumed.concurrency, LOW_PRIORITY_CONCURRENCY,
        "a resume that names no priority reuses the run's own width, not the \
         handler's `Normal` default"
    );
    assert_eq!(
        runs.get_recorded(run_id).expect("run").concurrency,
        Some(LOW_PRIORITY_CONCURRENCY),
        "and leaves the row alone"
    );
}

/// Naming a priority buys a resume no new legality: `paused` is still the
/// only status with an edge back into `running`, and a refused resume still
/// re-paces nothing.
#[tokio::test]
async fn given_an_unresumable_run_when_resumed_at_a_priority_then_still_invalid_state() {
    for status in UNRESUMABLE {
        let harness = ControlHarness::with_run(status).await;

        let result = harness
            .control
            .resume(harness.run_id, TOKEN, Some(RunPriority::Low))
            .await;

        assert!(
            matches!(result, Err(DomainError::InvalidState)),
            "resuming a {status:?} run at a priority must be refused, got {:?}",
            result.map(|resumed| resumed.run_id)
        );
        let run = harness.runs.get_recorded(harness.run_id).expect("run");
        assert_eq!(run.status, status, "a refused transition writes nothing");
        assert_eq!(
            run.concurrency,
            Some(DEFAULT_CONCURRENCY),
            "including the width — a refused resume must not re-pace the run"
        );
    }
}

/// A refused resume must not re-pace the run it failed to revive: the width
/// travels inside the same guarded write as the status change, so a run
/// cancelled out from under this call keeps the width it was cancelled with.
#[tokio::test]
async fn given_a_run_cancelled_before_the_write_when_resumed_at_a_priority_then_the_width_is_untouched(
) {
    let (control, runs, run_id) = paused_run_at_width(DEFAULT_CONCURRENCY).await;
    runs.cancel_after_next_get(run_id);

    let result = control.resume(run_id, TOKEN, Some(RunPriority::Low)).await;

    assert!(matches!(result, Err(DomainError::InvalidState)));
    assert_eq!(
        runs.get_recorded(run_id).expect("run").concurrency,
        Some(DEFAULT_CONCURRENCY),
        "a resume that was refused re-paces nothing"
    );
}

#[tokio::test]
async fn given_a_run_paused_twice_when_resumed_again_then_both_pauses_are_banked() {
    // `paused_millis` accumulates across segments rather than replacing the
    // last pause. Getting this wrong is invisible on the first resume and
    // wrong on every one after it.
    let (control, runs, run_id) = paused_run(at(1, 10), 8_412, at(1, 40)).await;
    control
        .resume(run_id, TOKEN, None)
        .await
        .expect("first resume");
    assert!(runs.pause(run_id, at(2, 0)).await.expect("second pause"));
    let control = RunControlHandler::new(
        FakeAuth::Allowing,
        runs.clone(),
        FixedClock(at(2, 15)),
        RunRegistry::new(),
        DEFAULT_CONCURRENCY,
        LOW_PRIORITY_CONCURRENCY,
    );

    control
        .resume(run_id, TOKEN, None)
        .await
        .expect("second resume");

    assert_eq!(
        runs.get_recorded(run_id).expect("run").paused_millis,
        45 * 60 * 1000,
        "the second pause is added to the first, not substituted for it"
    );
}

#[tokio::test]
async fn given_a_resumed_run_when_its_status_is_read_then_active_millis_excludes_the_pause() {
    // The whole point of banking the pause: `active_millis` is elapsed wall
    // time minus it, so a run that sat paused for half an hour must not report
    // that half hour as time spent working.
    let (control, runs, run_id) = paused_run(at(1, 10), 8_412, at(1, 40)).await;
    control.resume(run_id, TOKEN, None).await.expect("resume");
    let status = GetRunStatusHandler::new(
        FakeAuth::Allowing,
        runs.clone(),
        FixedClock(at(1, 50)),
        RunRegistry::new(),
    );

    let run = status.get(run_id, TOKEN).await.expect("status");

    assert_eq!(
        run.active_millis,
        20 * 60 * 1000,
        "50 minutes elapsed since 01:00, of which 30 were spent paused"
    );
}

#[tokio::test]
async fn given_an_unresumable_run_when_resumed_then_invalid_state_and_the_row_is_untouched() {
    for status in UNRESUMABLE {
        let harness = ControlHarness::with_run(status).await;

        let result = harness.control.resume(harness.run_id, TOKEN, None).await;

        assert!(
            matches!(result, Err(DomainError::InvalidState)),
            "resuming a {status:?} run must be refused, got {:?}",
            result.map(|resumed| resumed.run_id)
        );
        assert_eq!(
            harness.recorded_status(),
            status,
            "a refused transition writes nothing"
        );
    }
}

#[tokio::test]
async fn given_an_unknown_run_when_resumed_then_not_found() {
    let harness = ControlHarness::with_run(RunStatus::Paused).await;

    let result = harness.control.resume(Uuid::new_v4(), TOKEN, None).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

#[tokio::test]
async fn given_an_unauthenticated_caller_when_resuming_then_unauthorized_not_invalid_state() {
    // Authentication runs ahead of the lookup here for the reason it does on
    // every other verb: a bad token must learn neither that the run exists nor
    // what state it is in.
    let mut harness = ControlHarness::with_run(RunStatus::Complete).await;
    harness.control = RunControlHandler::new(
        FakeAuth::Denying,
        harness.runs.clone(),
        FixedClock(t(3)),
        harness.registry.clone(),
        DEFAULT_CONCURRENCY,
        LOW_PRIORITY_CONCURRENCY,
    );

    let result = harness
        .control
        .resume(harness.run_id, "bad-token", None)
        .await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

#[tokio::test]
async fn given_a_run_closed_between_the_lookup_and_the_write_when_resumed_then_invalid_state() {
    // The resume half of the race every verb now guards: the row reads
    // `paused` at the lookup and is cancelled by someone else before this
    // call's write lands. Unguarded, the resume would revive a run its owner
    // had just abandoned.
    let harness = ControlHarness::with_run(RunStatus::Paused).await;
    harness.runs.cancel_after_next_get(harness.run_id);

    let result = harness.control.resume(harness.run_id, TOKEN, None).await;

    assert!(
        matches!(result, Err(DomainError::InvalidState)),
        "a resume whose write is refused must be reported, not silently swallowed"
    );
    assert_eq!(harness.recorded_status(), RunStatus::Cancelled);
}

#[tokio::test]
async fn given_a_run_completed_between_the_lookup_and_the_write_when_cancelled_then_invalid_state()
{
    // The cancel-side mirror of the pause guard. The control call reads
    // `running`, the walk writes `finish`, and the cancel lands last. Without
    // a guard it rewrites a run that finished all of its work into a
    // `cancelled` one with a fresh finish time, and answers `Ok` — a
    // misreport, not a corruption, which is exactly why nothing else catches
    // it.
    let harness = ControlHarness::with_run(RunStatus::Running).await;
    harness.runs.complete_after_next_get(harness.run_id);

    let result = harness.control.cancel(harness.run_id, TOKEN).await;

    assert!(
        matches!(result, Err(DomainError::InvalidState)),
        "a cancel whose write is refused must be reported, not answered Ok; got {result:?}"
    );
    assert_eq!(
        harness.recorded_status(),
        RunStatus::Complete,
        "the completion stands — the late cancel must not have overwritten it"
    );
}

#[tokio::test]
async fn given_a_cancelled_run_when_a_walks_cancel_lands_afterwards_then_the_fake_keeps_its_tally()
{
    // The fake must mirror the adapter's *wider* guard on the tally branch,
    // or the handler tests would be passing against a repository that drops a
    // cancelled run's counts where the real one keeps them.
    let runs = FakeCatalogRunRepository::new();
    let run_id = Uuid::new_v4();
    runs.start(
        run_id,
        RunKind::Index,
        Some("/library"),
        t(1),
        DEFAULT_CONCURRENCY,
    )
    .await
    .expect("start");
    assert!(runs
        .cancel(run_id, None, t(2))
        .await
        .expect("control cancel"));

    let counts = RunCounts::Index {
        scanned: 13,
        indexed: 4,
        skipped: 1,
        already_cataloged: 0,
        failed: 1,
    };
    let applied = runs
        .cancel(run_id, Some(counts), t(3))
        .await
        .expect("walk cancel");

    assert!(applied);
    let run = runs.get_recorded(run_id).expect("run");
    assert_eq!(run.status, RunStatus::Cancelled);
    assert_eq!(run.counts, Some(counts), "the walk's tally is kept");
}
