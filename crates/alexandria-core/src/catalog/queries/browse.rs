use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::model::{File, FileType, FileView, StateFilter};
use crate::catalog::repos::CatalogRepository;
use crate::errors::DomainError;

/// Filter for the browse-files list query (UC-03 / FR-FC-12): type,
/// lifecycle state, and containing collection. The default (`file_type =
/// None`, `state = Active`, `collection_uuid = None`) excludes soft-deleted
/// records per the use case's main-flow step 2 and applies no collection
/// filter.
///
/// A `collection_uuid` that does not resolve to any collection matches no
/// files (an empty list) rather than an error — the same way a `file_type`
/// that matches no rows returns empty, not a rejection. Unlike `file_type`
/// and `state`, a collection UUID is a reference rather than an enum, so
/// there is nothing to validate as "recognised" the way UC-03 AF-03 checks
/// those two.
#[derive(Debug, Clone, Default)]
pub struct FileFilter {
    pub file_type: Option<FileType>,
    pub state: StateFilter,
    pub collection_uuid: Option<Uuid>,
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

    pub fn with_collection(mut self, collection_uuid: Uuid) -> Self {
        self.collection_uuid = Some(collection_uuid);
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
            .list_filtered(filter.file_type, filter.state, filter.collection_uuid)
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

        // Issue #44 image slice: width/height live outside `SubtypeMetadata`
        // (see `find_image_dimensions`'s doc comment), so they're fetched
        // separately and only for image files.
        let (width, height) = if file.file_type == FileType::Image {
            match self.repo.find_image_dimensions(uuid).await? {
                Some((w, h)) => (Some(w), Some(h)),
                None => (None, None),
            }
        } else {
            (None, None)
        };

        // Issue #44 document slice: page_count lives outside
        // `SubtypeMetadata` (see `find_document_page_count`'s doc comment),
        // so it's fetched separately and only for document files.
        let page_count = if file.file_type == FileType::Document {
            self.repo.find_document_page_count(uuid).await?
        } else {
            None
        };

        // Issue #44 video slice: duration_seconds lives outside
        // `SubtypeMetadata` (see `find_video_duration`'s doc comment), so
        // it's fetched separately and only for video files.
        let duration_seconds = if file.file_type == FileType::Video {
            self.repo.find_video_duration(uuid).await?
        } else {
            None
        };

        // Issue #44 comic slice: comic_page_count lives outside
        // `SubtypeMetadata` (see `find_comic_page_count`'s doc comment),
        // so it's fetched separately and only for comic files.
        let comic_page_count = if file.file_type == FileType::Comic {
            self.repo.find_comic_page_count(uuid).await?
        } else {
            None
        };

        Ok(FileView {
            file,
            metadata,
            width,
            height,
            page_count,
            duration_seconds,
            comic_page_count,
        })
    }
}
