use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::model::FileType;
use crate::catalog::repos::CatalogRepository;
use crate::errors::DomainError;
use crate::reading_lists::model::{ReadingProgress, ReadingTargetKind};
use crate::reading_lists::repos::ReadingListRepository;

/// UC-28 — Add an item to a reading list (FR-RL-02, FR-RL-03). Links a
/// `Document` (book) or `ComicBook` to a reading list, creating a `Pending`
/// ReadingProgress the first time; adding an already-linked item is
/// idempotent and returns the existing progress unchanged rather than
/// resetting it (no governing AF — chosen to avoid clobbering UC-29
/// progress, mirroring `AddVideoToWatchlistHandler`).
///
/// Generic over two repositories: `ReadingListRepository` to look up the
/// target reading list and perform the link, `CatalogRepository` to look up
/// the item and confirm its type. No `Clock` or `Filesystem` collaborator.
pub struct AddItemToReadingListHandler<A, RLR, CATR> {
    auth: A,
    reading_list_repo: RLR,
    catalog_repo: CATR,
}

impl<A, RLR, CATR> AddItemToReadingListHandler<A, RLR, CATR>
where
    A: AuthService,
    RLR: ReadingListRepository,
    CATR: CatalogRepository,
{
    pub fn new(auth: A, reading_list_repo: RLR, catalog_repo: CATR) -> Self {
        Self {
            auth,
            reading_list_repo,
            catalog_repo,
        }
    }

    /// Link the item identified by `item_uuid` to the reading list
    /// identified by `reading_list_uuid`.
    pub async fn add(
        &self,
        reading_list_uuid: Uuid,
        item_uuid: Uuid,
        token: &str,
    ) -> Result<ReadingProgress, DomainError> {
        // AF-03: the caller must be authenticated. Evaluated before the
        // reading list or item is looked up (FR-AU-07 / SRD §7).
        self.auth.authenticate(token).await?;

        // AF-02: the reading list must exist.
        self.reading_list_repo
            .find_by_uuid(reading_list_uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        // AF-02: the item must exist.
        let file = self
            .catalog_repo
            .find_by_uuid(item_uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        // AF-01: the target file must be a Document or ComicBook.
        let target_kind = match file.file_type {
            FileType::Document => ReadingTargetKind::Document,
            FileType::Comic => ReadingTargetKind::Comic,
            _ => {
                return Err(DomainError::InvalidInput(format!(
                    "file {item_uuid} is neither a document nor a comic"
                )))
            }
        };

        self.reading_list_repo
            .add_item(reading_list_uuid, item_uuid, target_kind)
            .await
    }
}
