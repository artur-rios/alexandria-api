use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::model::{File, FileType, FileView, StateFilter};
use crate::catalog::repos::CatalogRepository;
use crate::errors::DomainError;

/// Filter for the browse-files list query (UC-03 / FR-FC-12): type and
/// lifecycle state. The default (`file_type = None`, `state = Active`)
/// excludes soft-deleted records per the use case's main-flow step 2.
///
/// The collection filter is deferred until the Collections table lands
/// (UC-10+); only type and state filters are supported today.
#[derive(Debug, Clone, Default)]
pub struct FileFilter {
    pub file_type: Option<FileType>,
    pub state: StateFilter,
}

impl FileFilter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_type(mut self, file_type: FileType) -> Self {
        self.file_type = Some(file_type);
        self
    }

    pub fn with_state(mut self, state: StateFilter) -> Self {
        self.state = state;
        self
    }
}

/// Browse and view file metadata (UC-03 / FR-FC-12, FR-FC-13).
///
/// `list` authenticates the caller (AF-02), applies the filter, and returns
/// the matching files. The default state filter is `Active` — soft-deleted
/// records are excluded unless the owner explicitly requests them (main-flow
/// step 2). `get_by_uuid` authenticates the caller, looks up a single file by
/// its public UUID (AF-01 when absent), and returns the file plus its stored
/// subtype metadata when the subtype has one.
///
/// Generic over the auth service and catalog repository so the same decision
/// logic is unit-tested against trait fakes (no real DB or auth service in
/// unit tests), then wired with the concrete Bearer/Sqlite collaborators at
/// runtime (services.rs). Both the HTTP and FFI surfaces call this handler so
/// the two stay at parity (FR-FC-24 / NFR-09).
pub struct BrowseFilesHandler<A, R> {
    auth: A,
    repo: R,
}

impl<A, R> BrowseFilesHandler<A, R>
where
    A: AuthService,
    R: CatalogRepository,
{
    pub fn new(auth: A, repo: R) -> Self {
        Self { auth, repo }
    }

    /// List files matching `filter`. The default filter excludes
    /// soft-deleted records (UC-03 main-flow step 2).
    pub async fn list(&self, filter: FileFilter, token: &str) -> Result<Vec<File>, DomainError> {
        // AF-02: the caller must be authenticated.
        self.auth.authenticate(token).await?;
        self.repo
            .list_filtered(filter.file_type, filter.state)
            .await
    }

    /// Get a single file by its public UUID, including its stored subtype
    /// metadata when the subtype has one (FR-FC-13). AF-01 when the UUID
    /// does not exist.
    pub async fn get_by_uuid(&self, uuid: Uuid, token: &str) -> Result<FileView, DomainError> {
        // AF-02: the caller must be authenticated.
        self.auth.authenticate(token).await?;

        // AF-01: the file must exist.
        let file = self
            .repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        let metadata = self.repo.find_metadata_by_uuid(uuid).await?;

        Ok(FileView { file, metadata })
    }
}
