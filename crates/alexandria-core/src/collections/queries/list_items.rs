use uuid::Uuid;

use crate::bookmarks::repos::BookmarkRepository;
use crate::catalog::model::StateFilter;
use crate::catalog::repos::CatalogRepository;
use crate::collections::model::{CollectionItems, CollectionKind, CollectionMembersResult};
use crate::collections::repos::CollectionRepository;
use crate::errors::DomainError;

/// UC-14 (list) — List the items in a collection (FR-CO-07).
///
/// Generic over the same three repositories `AddItemsToCollectionHandler`
/// uses: which one supplies the membership list depends on the collection's
/// `kind`. Both repositories' `list_filtered` are used with the default
/// `StateFilter::Active`, matching UC-03's own default (soft-deleted members
/// excluded unless explicitly requested — which this read path does not yet
/// support).
pub struct ListCollectionItemsHandler<CR, CATR, BR> {
    collection_repo: CR,
    catalog_repo: CATR,
    bookmark_repo: BR,
}

impl<CR, CATR, BR> ListCollectionItemsHandler<CR, CATR, BR>
where
    CR: CollectionRepository,
    CATR: CatalogRepository,
    BR: BookmarkRepository,
{
    pub fn new(collection_repo: CR, catalog_repo: CATR, bookmark_repo: BR) -> Self {
        Self {
            collection_repo,
            catalog_repo,
            bookmark_repo,
        }
    }

    /// List the current members of the collection identified by
    /// `collection_uuid`.
    pub async fn list(
        &self,
        collection_uuid: Uuid,
    ) -> Result<CollectionMembersResult, DomainError> {
        // AF-01 (per the use case's own read path): the collection must
        // exist.
        let collection = self
            .collection_repo
            .find_by_uuid(collection_uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        let items = match collection.kind {
            CollectionKind::File => CollectionItems::Files(
                self.catalog_repo
                    // Across every library: a collection gathers files from
                    // wherever the owner keeps them.
                    .list_filtered(None, StateFilter::Active, Some(collection_uuid), None)
                    .await?,
            ),
            CollectionKind::Bookmark => CollectionItems::Bookmarks(
                self.bookmark_repo
                    .list_filtered(Some(collection_uuid), StateFilter::Active)
                    .await?,
            ),
        };

        Ok(CollectionMembersResult {
            collection_uuid,
            kind: collection.kind,
            items,
        })
    }
}
