use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::clock::Clock;
use crate::catalog::run_registry::{RunRegistry, RunSignal};
use crate::catalog::runs::{CatalogRunRepository, RunStatus};
use crate::errors::DomainError;
use crate::retry::{retry_on_busy, BUSY_ATTEMPTS};

/// Pause or cancel an index or re-index run in flight (UC-42).
///
/// A run answers immediately with an id and then walks a library for minutes
/// (FR-FC-08). Until now the only way to stop one was to stop the process.
/// This is the other end of the run registry Task 6 built: the walk publishes
/// its progress into a cell, and this handler writes a signal back into the
/// same cell, which the walk reads before every entry.
///
/// Why a signal rather than aborting the task: the walk owns a tally and a
/// row, and dropping it mid-flight would leave both half-written. Letting the
/// in-flight window drain costs milliseconds — per-file work is a stat and a
/// header read, not a full-file hash (FR-FC-09/FR-FC-10) — and buys a run
/// that records its own stopping point exactly once, in the same place it
/// would have recorded its completion.
///
/// Generic over the auth service, the run repository, and the clock, then
/// wired with the concrete Runtime/Sqlite/System collaborators at runtime
/// (services.rs). The clock is needed for the one case that writes the row
/// from here — a run with no live cell, below. The registry is concrete: a
/// process-local map of atomics with no I/O behind it, so there is nothing to
/// fake.
pub struct RunControlHandler<A, RR, C> {
    auth: A,
    runs: RR,
    clock: C,
    registry: RunRegistry,
}

/// Which verb was asked for. Private, and deliberately not [`RunSignal`]:
/// `RunSignal::None` is not a request a caller can make, and modelling it
/// here would force an unreachable arm into every match below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verb {
    Pause,
    Cancel,
}

impl Verb {
    fn signal(self) -> RunSignal {
        match self {
            Verb::Pause => RunSignal::Pause,
            Verb::Cancel => RunSignal::Cancel,
        }
    }

    /// Whether a run in `status` may be asked to do this.
    ///
    /// Only a `Running` run can be paused — pausing a paused run is a
    /// no-op the caller should be told about rather than silently accepted,
    /// and the three terminal statuses have no run left to stop.
    ///
    /// Cancel additionally accepts a `Paused` run: abandoning one is the
    /// whole reason to cancel rather than pause, and without it a paused run
    /// could never be got rid of.
    fn permits(self, status: RunStatus) -> bool {
        match self {
            Verb::Pause => status == RunStatus::Running,
            Verb::Cancel => matches!(status, RunStatus::Running | RunStatus::Paused),
        }
    }
}

impl<A, RR, C> RunControlHandler<A, RR, C>
where
    A: AuthService,
    RR: CatalogRunRepository,
    C: Clock,
{
    pub fn new(auth: A, runs: RR, clock: C, registry: RunRegistry) -> Self {
        Self {
            auth,
            runs,
            clock,
            registry,
        }
    }

    /// Stop a running run where it is, leaving it resumable.
    pub async fn pause(&self, run_id: Uuid, token: &str) -> Result<(), DomainError> {
        self.control(run_id, token, Verb::Pause).await
    }

    /// Abandon a run. Terminal — a cancelled run is not resumed.
    pub async fn cancel(&self, run_id: Uuid, token: &str) -> Result<(), DomainError> {
        self.control(run_id, token, Verb::Cancel).await
    }

    async fn control(&self, run_id: Uuid, token: &str, verb: Verb) -> Result<(), DomainError> {
        // AF-02, and first: everything after this point discloses something
        // about the run — whether it exists, and what state it is in. A
        // caller with a bad token must learn neither, so authentication runs
        // ahead of the lookup, not alongside it.
        self.auth.authenticate(token).await?;
        // AF-01: an id naming no run.
        let run = self.runs.get(run_id).await?.ok_or(DomainError::NotFound)?;
        if !verb.permits(run.status) {
            return Err(DomainError::InvalidState);
        }

        if let Some(cell) = self.registry.get(run_id) {
            // This process is executing the run. Raising the signal is the
            // whole job: the walk writes its own row once its in-flight
            // window has drained and its final tally is flushed. Writing the
            // row from here as well would race that write, and the walk's is
            // the one that knows how far it actually got.
            cell.raise(verb.signal());
            return Ok(());
        }

        // No live cell: nothing in this process is executing this run, so
        // there is no loop to write the row and this call has to. In practice
        // that is a `paused` run being cancelled, or the brief window in
        // which a walk has closed its cell but not yet written its own
        // terminal row.
        //
        // Pause is still recorded as a pause here rather than refused or
        // escalated to a cancel: the run has already stopped, so the only
        // question left is what it may become, and `paused` is the answer
        // that keeps the owner's options open. Refusing would leave a
        // `running` row nothing will ever advance, and cancelling would throw
        // away a resume the owner did not ask to give up.
        //
        // About that window, precisely — because the obvious claim, that
        // whichever write lands second wins and both orders are fine, is
        // wrong. A `cancel` landing here while a walk is between dropping its
        // cell and recording its own `pause` must not then be overwritten by
        // that pause: `pause`'s SQL touches neither `finished_at` nor `phase`,
        // so the row would end up `paused` with a finish time already stamped,
        // and a run the owner asked to abandon would look resumable.
        // `RunCell::raise`'s no-downgrade guard cannot help — the cell is
        // already gone. What holds the line is `pause` being conditional on
        // the row still reading `running`, so the late pause is refused and
        // the cancel stands. The reverse order is unproblematic: this call
        // would have read `cancelled`/`complete` at its lookup above and
        // returned `InvalidState`.
        //
        // Unlike the walk's own best-effort bookkeeping, a failure here is
        // reported: the caller asked for the run to stop, and must not be
        // told it did when the row says otherwise.
        let now = self.clock.now();
        match verb {
            Verb::Pause => {
                let applied = retry_on_busy(BUSY_ATTEMPTS, || self.runs.pause(run_id, now)).await?;
                if !applied {
                    // The run stopped being `running` between the lookup above
                    // and this write. Reporting the transition as refused is
                    // the honest answer, and the same one the caller would
                    // have got had it arrived a moment later.
                    return Err(DomainError::InvalidState);
                }
            }
            // `None`: this caller holds no partial tally. The walk that does
            // passes its own through `record_halt`.
            Verb::Cancel => {
                retry_on_busy(BUSY_ATTEMPTS, || self.runs.cancel(run_id, None, now)).await?
            }
        }
        Ok(())
    }
}
