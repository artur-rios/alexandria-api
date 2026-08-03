use chrono::Duration;
use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::clock::Clock;
use crate::catalog::model::{File, FileState};
use crate::catalog::repos::CatalogRepository;
use crate::errors::DomainError;

/// UC-07 — Restore a soft-deleted file (FR-FC-21). Returns a soft-deleted
/// file to `active` and clears `deleted_at`. The on-disk file is untouched;
/// only the catalog row's `state` and `deleted_at` change.
///
/// Like the soft-delete handler the command is the handler itself (no
/// separate `Command` struct): construction wires the collaborators
/// (`AuthService`, `CatalogRepository`, `Clock`) plus the configured
/// soft-delete retention window in days (`retention_days`, NFR-10; default
/// 30 in `Settings::deletion::retention_days`). The `restore` method is the
/// domain entry point. The clock is taken (rather than reading `SystemClock`
/// directly) so the retention-window boundaries are unit-testable with
/// `FixedClock`. Restore touches no filesystem, so there is no
/// `Filesystem` collaborator to compensate on failure.
///
/// Retention is **inclusive**: a file whose `deleted_at` is exactly
/// `retention_days` ago is still restorable. Strictly past it (one tick
/// more) is reported as `NotFound` (UC-08 owns the actual hard purge; before
/// then the row still exists, so the elapsed check is what UC-07 uses to
/// decide `NotFound`).
pub struct RestoreFileHandler<A, R, C> {
    auth: A,
    repo: R,
    clock: C,
    retention_days: u32,
}

impl<A, R, C> RestoreFileHandler<A, R, C>
where
    A: AuthService,
    R: CatalogRepository,
    C: Clock,
{
    pub fn new(auth: A, repo: R, clock: C, retention_days: u32) -> Self {
        Self {
            auth,
            repo,
            clock,
            retention_days,
        }
    }

    /// Restore `uuid` to `active` and return the re-read `File` (carrying
    /// the persisted `state = active` and a cleared `deleted_at`).
    pub async fn restore(&self, uuid: Uuid, token: &str) -> Result<File, DomainError> {
        // AF-03: the caller must be authenticated. Evaluation happens before
        // any payload is consulted (FR-AU-07 / SRD §7), so an unauthenticated
        // caller learns nothing about whether the uuid exists — consistent
        // with the auth-before-payload ordering of the other file-lifecycle
        // handlers.
        self.auth.authenticate(token).await?;

        // AF-02 (missing): the file must exist.
        let file = self
            .repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        // AF-02 (not-deleted): the file-lifecycle state diagram (Use Case
        // Spec §4.1) only allows `deleted → active`; restoring an already
        // `active` record is rejected as a state conflict (409 over HTTP /
        // FILE_ERR_INVALID_STATE over FFI) so the caller is not handed back
        // an "restored" row that was never deleted.
        if file.state != FileState::Deleted {
            return Err(DomainError::InvalidState);
        }

        // A `deleted` row should always carry a `deleted_at` (UC-06 stamped
        // it). A `deleted` row without one is corrupt data; we surface it
        // as `InvalidState` rather than silently treating the row as
        // restorable (which would let the retention check be skipped).
        let deleted_at = file.deleted_at.ok_or(DomainError::InvalidState)?;

        // AF-01: the record was already hard-purged (past retention). UC-08
        // owns the actual hard purge; before it runs the row still exists,
        // so elapsed-check here is what makes a past-retention restore
        // report `NotFound`. The boundary is inclusive — `now - deleted_at
        // == retention_days` is the last restorable day, one tick more is
        // past retention.
        let elapsed = self.clock.now() - deleted_at;
        if elapsed > Duration::days(i64::from(self.retention_days)) {
            return Err(DomainError::NotFound);
        }

        self.repo.restore(uuid).await
    }
}