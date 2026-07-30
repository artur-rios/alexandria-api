use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::model::{FileMetadata, FileState, SubtypeMetadata};
use crate::catalog::repos::CatalogRepository;
use crate::errors::DomainError;

/// Edit a file's type-specific metadata (UC-04 / FR-FC-14..18).
///
/// The handler authenticates the caller (AF-03), looks the file up by its
/// public UUID (AF-02 when absent), rejects edits on a soft-deleted record
/// (AF-04 — restore first via UC-07), and validates that the metadata variant
/// matches the file's actual subtype, including rejecting text/html files
/// which have no editable subtype metadata (AF-01). On success the editable
/// subtype columns are fully replaced (a PATCH is a full replace — `None`
/// writes `NULL`) and the updated file plus its written metadata are returned.
///
/// Generic over the auth service and catalog repository so the same decision
/// logic is unit-tested against trait fakes (no real DB or auth service in
/// unit tests), then wired with the concrete Bearer/Sqlite collaborators at
/// runtime (services.rs). Both the HTTP and FFI surfaces call this handler so
/// the two stay at parity (FR-FC-24 / NFR-09).
pub struct EditMetadataHandler<A, R> {
    auth: A,
    repo: R,
}

impl<A, R> EditMetadataHandler<A, R>
where
    A: AuthService,
    R: CatalogRepository,
{
    pub fn new(auth: A, repo: R) -> Self {
        Self { auth, repo }
    }

    /// Apply `metadata` to the file identified by `uuid`.
    ///
    /// Returns the updated `FileMetadata` (file + written metadata) on
    /// success. The file record's state, path, name, hash, and timestamps are
    /// not touched — only the editable subtype columns change.
    pub async fn edit(
        &self,
        uuid: Uuid,
        metadata: SubtypeMetadata,
        token: &str,
    ) -> Result<FileMetadata, DomainError> {
        // AF-03: the caller must be authenticated.
        self.auth.authenticate(token).await?;

        // AF-02: the file must exist.
        let file = self
            .repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        // AF-04: editing a soft-deleted file is rejected; restore first (UC-07).
        if file.state == FileState::Deleted {
            return Err(DomainError::InvalidState);
        }

        // AF-01: the metadata variant must match the file's actual subtype.
        // This also covers Text/Html files, which have no editable subtype
        // metadata — no SubtypeMetadata variant maps to them, so any PATCH
        // body's variant will mismatch the file's `FileType`.
        if metadata.file_type() != file.file_type {
            return Err(DomainError::InvalidInput(
                "metadata does not match file subtype".into(),
            ));
        }

        self.repo.update_metadata(uuid, &metadata).await?;

        // Echo the cataloged file and the metadata we just wrote (no extra DB
        // read: a PATCH is a full replace, so what we sent is what's stored).
        Ok(FileMetadata { file, metadata })
    }
}