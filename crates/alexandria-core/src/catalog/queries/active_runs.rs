use crate::auth::AuthService;
use crate::catalog::clock::Clock;
use crate::catalog::queries::run_status::overlay_live_state;
use crate::catalog::run_registry::RunRegistry;
use crate::catalog::runs::{CatalogRun, CatalogRunRepository};
use crate::errors::DomainError;

/// UC-42 — Query every outstanding run at once (FR-FC-35).
///
/// `GetRunStatusHandler` answers "what became of *this* run," which is fine
/// when a caller already holds a run id. Two things the front end needs
/// cannot be answered that way: a global "something is indexing" indicator,
/// and an offer to resume at launch. Both need one question answered across
/// *all* runs at once — the front end tracking run ids in its own settings
/// store cannot answer either, because it only knows about the folders it
/// happens to remember, and it has no way to notice a run this process
/// itself paused at startup (FR-FC-29). The core is the only place that
/// holds every run, so it is the only place that can answer honestly.
///
/// "Outstanding" means [`RunStatus::Running`](crate::catalog::runs::RunStatus::Running)
/// or [`RunStatus::Paused`](crate::catalog::runs::RunStatus::Paused) — the
/// two non-terminal states. `Complete`, `Failed`, and `Cancelled` are all
/// finished, whichever way they finished, and none of them are what a
/// "something is still going" indicator or a resume offer is for.
///
/// Generic over the auth service, the run repository, and the clock for the
/// same reason as `GetRunStatusHandler`: the decision logic is unit-tested
/// against trait fakes, then wired with the concrete
/// Runtime/Sqlite/System collaborators at runtime (services.rs).
pub struct GetActiveRunsHandler<A, RR, C> {
    auth: A,
    runs: RR,
    clock: C,
    registry: RunRegistry,
}

impl<A, RR, C> GetActiveRunsHandler<A, RR, C>
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

    /// Every outstanding run, newest first, each with live progress
    /// overlaid exactly as [`GetRunStatusHandler::get`] overlays a single
    /// run — a caller listing outstanding runs wants current numbers, not
    /// the last flush.
    ///
    /// [`GetRunStatusHandler::get`]: crate::catalog::queries::run_status::GetRunStatusHandler::get
    pub async fn list(&self, token: &str) -> Result<Vec<CatalogRun>, DomainError> {
        // AF-02: every catalog operation authenticates the owner.
        self.auth.authenticate(token).await?;
        // No outstanding runs is the normal case for an idle library, not an
        // error — the repository already answers with an empty list rather
        // than `NotFound`, and there is nothing here that would turn that
        // into one.
        let mut runs = self.runs.list_active().await?;
        for run in &mut runs {
            overlay_live_state(run, &self.registry, &self.clock);
        }
        Ok(runs)
    }
}
