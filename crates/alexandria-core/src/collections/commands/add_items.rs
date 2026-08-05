use uuid::Uuid;

use crate::auth::AuthService;
use crate::bookmarks::repos::BookmarkRepository;
use crate::catalog::repos::CatalogRepository;
use crate::collections::model::{CollectionItemsResult, CollectionKind};
use crate::collections::repos::CollectionRepository;
use crate::errors::DomainError;

/// UC-13 — Add items to a collection (FR-CO-05). Links one or more items of
/// the matching `kind` to a collection.
///
/// Generic over three repositories: `CollectionRepository` to look up the
/// target collection and its `kind`; `CatalogRepository` and
/// `BookmarkRepository` to check an item's existence and perform the link,
/// depending on whether the collection is `kind = file` or `kind = bookmark`.
/// There is no `Clock` or `Filesystem` collaborator — linking touches no
/// timestamps and nothing on disk.
pub struct AddItemsToCollectionHandler<A, CR, CATR, BR> {
    auth: A,
    collection_repo: CR,
    catalog_repo: CATR,
    bookmark_repo: BR,
}

impl<A, CR, CATR, BR> AddItemsToCollectionHandler<A, CR, CATR, BR>
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

    /// Add `item_uuids` to the collection identified by `collection_uuid`.
    /// Every item is validated before any is linked, so the request either
    /// links all of them or none (AF-01's "rejects the entire request").
    pub async fn add(
        &self,
        collection_uuid: Uuid,
        item_uuids: Vec<Uuid>,
        token: &str,
    ) -> Result<CollectionItemsResult, DomainError> {
        // AF-04: the caller must be authenticated. Evaluated before the
        // collection or items are looked up (FR-AU-07 / SRD §7).
        self.auth.authenticate(token).await?;

        // AF-03: the collection must exist.
        let collection = self
            .collection_repo
            .find_by_uuid(collection_uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        // Validate every item before linking any. Existence in the *other*
        // table distinguishes AF-01 (wrong kind) from AF-02 (does not exist
        // at all): the caller submits bare uuids with no declared kind, so a
        // uuid that resolves in the collection's own table is fine, one that
        // resolves only in the other table is a type mismatch, and one that
        // resolves in neither does not exist.
        for uuid in &item_uuids {
            match collection.kind {
                CollectionKind::File => {
                    if self.catalog_repo.find_by_uuid(*uuid).await?.is_some() {
                        continue;
                    }
                    if self.bookmark_repo.find_by_uuid(*uuid).await?.is_some() {
                        return Err(DomainError::InvalidInput(format!(
                            "item {uuid} is a bookmark, not a file"
                        )));
                    }
                    return Err(DomainError::NotFound);
                }
                CollectionKind::Bookmark => {
                    if self.bookmark_repo.find_by_uuid(*uuid).await?.is_some() {
                        continue;
                    }
                    if self.catalog_repo.find_by_uuid(*uuid).await?.is_some() {
                        return Err(DomainError::InvalidInput(format!(
                            "item {uuid} is a file, not a bookmark"
                        )));
                    }
                    return Err(DomainError::NotFound);
                }
            }
        }

        for uuid in &item_uuids {
            match collection.kind {
                CollectionKind::File => {
                    self.catalog_repo
                        .set_collection(*uuid, collection_uuid)
                        .await?
                }
                CollectionKind::Bookmark => {
                    self.bookmark_repo
                        .set_collection(*uuid, collection_uuid)
                        .await?
                }
            }
        }

        Ok(CollectionItemsResult {
            collection_uuid,
            item_uuids,
        })
    }
}
