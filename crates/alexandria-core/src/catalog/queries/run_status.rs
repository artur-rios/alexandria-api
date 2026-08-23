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

        overlay_live_state(&mut run, &self.registry, &self.clock);

        Ok(run)
    }
}

/// Overlay a persisted run with its live registry cell (if any) and compute
/// `active_millis` — the two things a caller reading run state wants that the
/// stored row alone cannot answer. Shared by [`GetRunStatusHandler::get`] and
/// `GetActiveRunsHandler::list` (`catalog::queries::active_runs`): a client
/// listing outstanding runs wants the same current numbers a single-run query
/// gives, not the last flush, and the `active_millis` arithmetic below is
/// subtle enough — see the paused-clock note — that a second copy of it would
/// only ever be the wrong one.
pub(crate) fn overlay_live_state<C: Clock>(
    run: &mut CatalogRun,
    registry: &RunRegistry,
    clock: &C,
) {
    // A live cell outranks the row: the row holds the last flush, which
    // is by construction up to one flush interval stale.
    if let Some(cell) = registry.get(run.id) {
        let progress = cell.snapshot();
        run.phase = Some(progress.phase);
        run.total = progress.total;
        run.processed = Some(progress.processed);
    }

    // Time the run spent working: elapsed wall time, minus the time it
    // spent paused. A finished run's clock stops at `finished_at`; a
    // running one's keeps going, which is why this needs a clock at all.
    //
    // Two subtractions, not one. `paused_millis` holds the pauses that
    // have *ended* — only a resume banks one — so a run sitting paused
    // right now has a stretch that is in neither term, and without the
    // second one its `active_millis` would climb with the wall clock for
    // as long as it stayed paused. That is not a corner case since
    // startup reconciliation began pausing rather than closing runs: a
    // run left over from a previous launch is paused with no
    // `finished_at`, so its clock would run for every day the
    // application stayed shut, and a client dividing `processed` by this
    // to estimate what is left would get an answer that degrades the
    // longer the owner leaves the run alone.
    //
    // The open pause is measured to `elapsed_to` rather than to now, so
    // it freezes with everything else for a terminal run — a run
    // cancelled while paused keeps a `paused_at`, and measuring that one
    // to now would make its finished clock move again.
    //
    // Clamped at zero. The subtraction can go negative if the system
    // clock steps backwards mid-run, or if a resume over-accumulates
    // `paused_millis`, and "this run has been working for minus four
    // seconds" is nonsense on a display field — better to report no
    // elapsed time than a negative duration.
    let elapsed_to = run.finished_at.unwrap_or_else(|| clock.now());
    let open_pause = run
        .paused_at
        .map(|paused_at| (elapsed_to - paused_at).num_milliseconds().max(0))
        .unwrap_or(0);
    run.active_millis =
        ((elapsed_to - run.started_at).num_milliseconds() - run.paused_millis - open_pause).max(0);
}
