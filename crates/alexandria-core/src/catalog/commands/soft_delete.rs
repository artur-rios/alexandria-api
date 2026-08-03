use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::auth::AuthService;
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
/// (`AuthService`, `CatalogRepository`, `Clock`), the `soft_delete` method
/// is the domain entry point, and a `Clock` is taken (rather than reading
/// `SystemClock` directly) so unit tests can stamp a deterministic
/// `deleted_at` via `FixedClock` (see the rename handler's note on test
/// doubles). Soft-delete touches no filesystem, so there is no
/// `Filesystem` collaborator to compensate on failure.
pub struct SoftDeleteFileHandler<A, R, C> {
    auth: A,
    repo: R,
    clock: C,
}

impl<A, R, C> SoftDeleteFileHandler<A, R, C>
where
    A: AuthService,
    R: CatalogRepository,
    C: Clock,
{
    pub fn new(auth: A, repo: R, clock: C) -> Self {
        Self { auth, repo, clock }
    }

    /// Mark `uuid` soft-deleted and return the re-read `File` (carrying the
    /// persisted `state = deleted` and the stamped `deleted_at`).
    pub async fn soft_delete(
        &self,
        uuid: Uuid,
        token: &str,
    ) -> Result<File, DomainError> {
        // AF-02: the caller must be authenticated. Evaluation happens before
        // any payload is consulted (FR-AU-07 / SRD §7), so an unauthenticated
        // caller learns nothing about whether the uuid exists.
        self.auth.authenticate(token).await?;

        // AF-01: the file must exist.
        let file = self
            .repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        // The file-lifecycle state diagram (Use Case Spec §4.1) only allows
        // `active → deleted`; a record already `deleted` is restored via
        // UC-07, not re-soft-deleted. Re-issuing a soft-delete on an already
        // deleted record is rejected as a state conflict (409 over HTTP /
        // FILE_ERR_INVALID_STATE over FFI) so the caller is not silently
        // re-stamped with a later `deleted_at` (which would reset the
        // retention window without the caller asking).
        if file.state == FileState::Deleted {
            return Err(DomainError::InvalidState);
        }

        let now: DateTime<Utc> = self.clock.now();
        self.repo.soft_delete(uuid, now).await
    }
}