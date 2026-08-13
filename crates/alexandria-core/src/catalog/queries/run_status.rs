use uuid::Uuid;

use crate::auth::AuthService;
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
/// Generic over the auth service and the run repository so the decision logic
/// is unit-tested against trait fakes, then wired with the concrete
/// Runtime/Sqlite collaborators at runtime (services.rs).
pub struct GetRunStatusHandler<A, RR> {
    auth: A,
    runs: RR,
}

impl<A, RR> GetRunStatusHandler<A, RR>
where
    A: AuthService,
    RR: CatalogRunRepository,
{
    pub fn new(auth: A, runs: RR) -> Self {
        Self { auth, runs }
    }

    /// The recorded run for `run_id`.
    pub async fn get(&self, run_id: Uuid, token: &str) -> Result<CatalogRun, DomainError> {
        // AF-02: every catalog operation authenticates the owner.
        self.auth.authenticate(token).await?;
        // AF-01: an id naming no run.
        self.runs.get(run_id).await?.ok_or(DomainError::NotFound)
    }
}
