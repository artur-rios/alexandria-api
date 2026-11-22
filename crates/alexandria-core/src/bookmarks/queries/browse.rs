use uuid::Uuid;

use crate::bookmarks::model::Bookmark;
use crate::bookmarks::repos::BookmarkRepository;
use crate::catalog::model::StateFilter;
use crate::collections::repos::CollectionRepository;
use crate::errors::DomainError;

/// Filter for the browse-bookmarks list query (UC-17 / FR-BM-06): containing
/// collection and lifecycle state. The default (`collection_uuid = None`,
/// `state = Active`) applies no collection filter and excludes soft-deleted
/// records per the use case's main-flow step 2 — mirroring
/// `catalog::queries::browse::FileFilter`.
#[derive(Debug, Clone, Default)]
pub struct BookmarkFilter {
    pub collection_uuid: Option<Uuid>,
    pub state: StateFilter,
}

impl BookmarkFilter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_collection(mut self, collection_uuid: Uuid) -> Self {
        self.collection_uuid = Some(collection_uuid);
        self
    }

    pub fn with_state(mut self, state: StateFilter) -> Self {
        self.state = state;
        self
    }
}

/// Browse bookmarks, organized by bookmark collection (UC-17 / FR-BM-06).
///
/// Unlike `catalog::queries::browse::FileFilter`'s deferred collection filter,
/// a `collection_uuid` here that does not resolve to any collection is
/// `NotFound` (AF-01) rather than an empty list — the use case's own
/// alternative flow says so explicitly, where UC-03's FR-FC-12 filter left the
/// question open (see the comment on `FileFilter::collection_uuid`). Generic
/// over the bookmark repository and the collection repository so the same
/// decision logic is unit-tested against trait fakes, then wired with the
/// concrete Sqlite collaborators at runtime (services.rs). Both the HTTP and
/// FFI surfaces call this handler so the two stay at parity (FR-FC-24 /
/// NFR-09).
pub struct BrowseBookmarksHandler<BR, CR> {
    bookmark_repo: BR,
    collection_repo: CR,
}

impl<BR, CR> BrowseBookmarksHandler<BR, CR>
where
    BR: BookmarkRepository,
    CR: CollectionRepository,
{
    pub fn new(bookmark_repo: BR, collection_repo: CR) -> Self {
        Self {
            bookmark_repo,
            collection_repo,
        }
    }

    /// List bookmarks matching `filter`. The default filter excludes
    /// soft-deleted records (UC-17 main-flow step 2).
    pub async fn list(&self, filter: BookmarkFilter) -> Result<Vec<Bookmark>, DomainError> {
        // AF-01: a referenced collection must exist.
        if let Some(collection_uuid) = filter.collection_uuid {
            self.collection_repo
                .find_by_uuid(collection_uuid)
                .await?
                .ok_or(DomainError::NotFound)?;
        }

        self.bookmark_repo
            .list_filtered(filter.collection_uuid, filter.state)
            .await
    }
}
