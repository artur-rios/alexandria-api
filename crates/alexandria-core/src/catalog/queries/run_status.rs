use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::clock::Clock;
use crate::catalog::run_registry::RunRegistry;
use crate::catalog::runs::{CatalogRun, CatalogRunRepository};
use crate::errors::DomainError;

/// UC-42 — Query an index or refresh run (FR-FC-28).
///
/// Starting a run answers immediately with a run id (FR-FC-08 keeps runs
/// asynchronous); this is how the caller finds out what became of it. Without
/// it the only observable signals are the catalog counts, which say nothing
/// about whether a walk has finished — a client watching them can read a
/// half-finished run and not know.
///
/// It also answers *how far along* a run is. Two sources feed that answer, in
/// this order:
///   * the run's live cell in the [`RunRegistry`], when this process is
///     executing the run — exact, and up to date as of the read; and
///   * the row's own last flushed progress otherwise, which is what lets a
///     run this process is no longer executing still report its tally.
///
/// Generic over the auth service, the run repository, and the clock so the
/// decision logic is unit-tested against trait fakes, then wired with the
/// concrete Runtime/Sqlite/System collaborators at runtime (services.rs). The
/// registry is concrete: it is a process-local map of atomics with no I/O
/// behind it, so there is nothing to fake.
pub struct GetRunStatusHandler<A, RR, C> {
    auth: A,
    runs: RR,
    clock: C,
    registry: RunRegistry,
}

impl<A, RR, C> GetRunStatusHandler<A, RR, C>
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

    /// The recorded run for `run_id`, with live progress overlaid.
    pub async fn get(&self, run_id: Uuid, token: &str) -> Result<CatalogRun, DomainError> {
        // AF-02: every catalog operation authenticates the owner.
        self.auth.authenticate(token).await?;
        // AF-01: an id naming no run.
        let mut run = self.runs.get(run_id).await?.ok_or(DomainError::NotFound)?;

        // A live cell outranks the row: the row holds the last flush, which
        // is by construction up to one flush interval stale.
        if let Some(cell) = self.registry.get(run_id) {
            let progress = cell.snapshot();
            run.phase = Some(progress.phase);
            run.total = progress.total;
            run.processed = Some(progress.processed);
        }

        // Time the run spent working: elapsed wall time, minus the time it
        // spent paused. A finished run's clock stops at `finished_at`; a
        // running one's keeps going, which is why this needs a clock at all.
        //
        // Clamped at zero. The subtraction can go negative if the system
        // clock steps backwards mid-run, or if a future resume path
        // over-accumulates `paused_millis`, and "this run has been working
        // for minus four seconds" is nonsense on a display field — better to
        // report no elapsed time than a negative duration.
        let elapsed_to = run.finished_at.unwrap_or_else(|| self.clock.now());
        run.active_millis =
            ((elapsed_to - run.started_at).num_milliseconds() - run.paused_millis).max(0);

        Ok(run)
    }
}
