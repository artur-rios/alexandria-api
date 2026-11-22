use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::catalog::clock::Clock;
use crate::catalog::model::{File, FileState};
use crate::catalog::repos::CatalogRepository;
use crate::errors::DomainError;

/// UC-06 — Soft-delete a file (FR-FC-20). Marks the file record `deleted`,
/// hiding it from active views while keeping it restorable via UC-07. The
/// on-disk file is untouched; only the catalog row's `state` and
/// `deleted_at` change.
///
/// Like the other file-lifecycle handlers the command is the handler itself
/// (no separate `Command` struct): construction wires the collaborators
/// (`CatalogRepository`, `Clock`), the `soft_delete` method is the domain
/// entry point, and a `Clock` is taken (rather than reading `SystemClock`
/// directly) so unit tests can stamp a deterministic `deleted_at` via
/// `FixedClock` (see the rename handler's note on test doubles). Soft-delete
/// touches no filesystem, so there is no `Filesystem` collaborator to
/// compensate on failure.
pub struct SoftDeleteFileHandler<R, C> {
    repo: R,
    clock: C,
}

impl<R, C> SoftDeleteFileHandler<R, C>
where
    R: CatalogRepository,
    C: Clock,
{
    pub fn new(repo: R, clock: C) -> Self {
        Self { repo, clock }
    }

    /// Mark `uuid` soft-deleted and return the re-read `File` (carrying the
    /// persisted `state = deleted` and the stamped `deleted_at`).
    pub async fn soft_delete(&self, uuid: Uuid) -> Result<File, DomainError> {
        // AF-01: the file must exist.
        let file = self
            .repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        // The file-lifecycle state diagram (Use Case Spec §4.1) only allows
        // `active → deleted`; a record already `deleted` is restored via
        // UC-07, not re-soft-deleted. Re-issuing a soft-delete on an already
        // deleted record is rejected as a state conflict
        // (`FILE_ERR_INVALID_STATE` over FFI) so the caller is not silently
        // re-stamped with a later `deleted_at` (which would reset the
        // retention window without the caller asking).
        if file.state == FileState::Deleted {
            return Err(DomainError::InvalidState);
        }

        let now: DateTime<Utc> = self.clock.now();
        self.repo.soft_delete(uuid, now).await
    }
}
