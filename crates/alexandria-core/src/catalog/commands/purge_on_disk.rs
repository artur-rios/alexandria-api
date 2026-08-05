use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::fs::Filesystem;
use crate::catalog::model::PurgeOnDiskOutcome;
use crate::catalog::repos::CatalogRepository;
use crate::errors::DomainError;

/// UC-09 — Purge a file both on disk and in the catalog (FR-FC-23,
/// FR-FC-24). Unlike UC-08 there is no retention gate: the file's record may
/// be `active` or `deleted` — the only precondition is that it exists
/// (BR-11: purge-on-disk is a distinct operation from the retention-gated
/// hard purge and does not share its restriction).
///
/// Order of operations follows the spec's main flow: authenticate, look up
/// the record, delete the on-disk file, then remove the catalog row. Disk
/// deletion happens *before* the catalog write so a disk failure (AF-02)
/// leaves the record untouched — there is nothing to roll back because
/// nothing was written yet. The inverse ordering is not available: if the
/// disk delete succeeds and the subsequent catalog write fails, the file is
/// already gone and cannot be un-deleted. That error is surfaced as-is and
/// the row remains, now pointing at an absent path that a future UC-02
/// re-index will mark `missing`. The spec does not name this residual case;
/// this handler does not attempt a compensating write for it.
pub struct PurgeFileOnDiskHandler<A, R, F> {
    auth: A,
    repo: R,
    fs: F,
}

impl<A, R, F> PurgeFileOnDiskHandler<A, R, F>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
{
    pub fn new(auth: A, repo: R, fs: F) -> Self {
        Self { auth, repo, fs }
    }

    /// Purge `uuid`'s on-disk file and catalog row, returning a
    /// [`PurgeOnDiskOutcome`] confirming both: the pre-delete `File`
    /// snapshot and whether the on-disk file was actually present
    /// (`disk_file_present == false` is AF-01, still a success).
    pub async fn purge_on_disk(
        &self,
        uuid: Uuid,
        token: &str,
    ) -> Result<PurgeOnDiskOutcome, DomainError> {
        // AF-04: the caller must be authenticated, evaluated before any
        // payload is consulted (FR-AU-07 / SRD §7), matching the
        // auth-before-payload ordering of the other file-lifecycle handlers.
        self.auth.authenticate(token).await?;

        // AF-03: the file must exist.
        let file = self
            .repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        // AF-02: a disk failure aborts before any catalog write, so the
        // record is left exactly as it was.
        let disk_file_present = self.fs.remove_file(&file.path).await?;

        self.repo.purge(uuid).await?;

        Ok(PurgeOnDiskOutcome {
            file,
            disk_file_present,
        })
    }
}
