use uuid::Uuid;

use crate::auth::AuthService;
use crate::bookmarks::repos::BookmarkRepository;
use crate::catalog::repos::CatalogRepository;
use crate::collections::model::{CollectionItemResult, CollectionKind};
use crate::collections::repos::CollectionRepository;
use crate::errors::DomainError;

/// UC-14 (remove) — Remove an item from a collection (FR-CO-06). Unlinks a
/// single item, leaving both the item and the collection intact.
///
/// Generic over the same three repositories `AddItemsToCollectionHandler`
/// uses, for the same reason: which one performs the unlink depends on the
/// collection's `kind`.
pub struct RemoveItemFromCollectionHandler<A, CR, CATR, BR> {
    auth: A,
    collection_repo: CR,
    catalog_repo: CATR,
    bookmark_repo: BR,
}

impl<A, CR, CATR, BR> RemoveItemFromCollectionHandler<A, CR, CATR, BR>
where
    A: AuthService,
    CR: CollectionRepository,
    CATR: CatalogRepository,
    BR: BookmarkRepository,
{
    pub fn new(auth: A, collection_repo: CR, catalog_repo: CATR, bookmark_repo: BR) -> Self {
        Self {
            auth,
            collection_repo,
            catalog_repo,
            bookmark_repo,
        }
    }

    /// Remove `item_uuid` from the collection identified by
    /// `collection_uuid`.
    pub async fn remove(
        &self,
        collection_uuid: Uuid,
        item_uuid: Uuid,
        token: &str,
    ) -> Result<CollectionItemResult, DomainError> {
        // AF-03: the caller must be authenticated. Evaluated before the
        // collection or item are looked up (FR-AU-07 / SRD §7).
        self.auth.authenticate(token).await?;

        // AF-02: the collection must exist.
        let collection = self
            .collection_repo
            .find_by_uuid(collection_uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        // AF-01: the item must exist and currently be in this collection.
        // The repository's `clear_collection` defends both in one statement
        // (a zero-row update means either) since the specification maps
        // both to the same `NotFound`.
        match collection.kind {
            CollectionKind::File => {
                self.catalog_repo
                    .clear_collection(item_uuid, collection_uuid)
                    .await?
            }
            CollectionKind::Bookmark => {
                self.bookmark_repo
                    .clear_collection(item_uuid, collection_uuid)
                    .await?
            }
        }

        Ok(CollectionItemResult {
            collection_uuid,
            item_uuid,
        })
    }
}
