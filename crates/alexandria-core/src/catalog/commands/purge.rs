use chrono::Duration;
use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::clock::Clock;
use crate::catalog::model::{File, FileState};
use crate::catalog::repos::CatalogRepository;
use crate::errors::DomainError;

/// UC-08 — Hard-purge a file record (FR-FC-22). Permanently removes a
/// soft-deleted file's catalog row (and its subtype row) once the retention
/// window has elapsed. The on-disk file is untouched (NFR-07); only the
/// catalog rows are deleted.
///
/// Same shape as [`RestoreFileHandler`](crate::catalog::commands::restore::RestoreFileHandler):
/// construction wires the collaborators (`AuthService`, `CatalogRepository`,
/// `Clock`) plus the configured soft-delete retention window in days
/// (`retention_days`, NFR-10; default 30 in
/// `Settings::deletion::retention_days`). The `purge` method is the domain
/// entry point.
///
/// The retention boundary here is the exact complement of UC-07's: UC-07
/// restores while `elapsed <= retention_days` (inclusive), so UC-08 purges
/// only once `elapsed > retention_days` — at exactly `retention_days` the
/// record is still restorable and *not* yet purgeable.
pub struct PurgeFileHandler<A, R, C> {
    auth: A,
    repo: R,
    clock: C,
    retention_days: u32,
}

impl<A, R, C> PurgeFileHandler<A, R, C>
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

    /// Hard-purge `uuid`'s catalog row and return the `File` as it was
    /// immediately before deletion (a confirmation snapshot; the row itself
    /// no longer exists once this returns `Ok`).
    pub async fn purge(&self, uuid: Uuid, token: &str) -> Result<File, DomainError> {
        // AF-03: the caller must be authenticated, evaluated before any
        // payload is consulted (FR-AU-07 / SRD §7), matching the
        // auth-before-payload ordering of the other file-lifecycle handlers.
        self.auth.authenticate(token).await?;

        // AF-02: the file must exist.
        let file = self
            .repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        // AF-01: an `active` file never started a retention window, so it
        // can never be purgeable.
        if file.state != FileState::Deleted {
            return Err(DomainError::InvalidState);
        }

        // A `deleted` row should always carry a `deleted_at` (UC-06 stamped
        // it). A `deleted` row without one is corrupt data; surface it as
        // `InvalidState` rather than silently treating the row as purgeable.
        let deleted_at = file.deleted_at.ok_or(DomainError::InvalidState)?;

        // AF-01: only past-retention records are purgeable. The boundary is
        // the strict complement of UC-07's inclusive restore boundary —
        // `elapsed == retention_days` is still restorable and not yet
        // purgeable.
        let elapsed = self.clock.now() - deleted_at;
        if elapsed <= Duration::days(i64::from(self.retention_days)) {
            return Err(DomainError::InvalidState);
        }

        self.repo.purge(uuid).await?;
        Ok(file)
    }
}
