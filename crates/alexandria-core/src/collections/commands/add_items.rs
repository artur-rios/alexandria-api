use uuid::Uuid;

use crate::auth::AuthService;
use crate::bookmarks::repos::BookmarkRepository;
use crate::catalog::repos::CatalogRepository;
use crate::collections::model::{
    CollectionItemOutcome, CollectionItemsResult, CollectionKind, ItemRejection,
};
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

    /// Add `item_uuids` to the collection identified by `collection_uuid`,
    /// reporting what became of each.
    ///
    /// Links what it can and reports the rest. AF-01 (wrong kind) and AF-02
    /// (no such item) are per-item reasons rather than request-level errors:
    /// a caller that has to explain the outcome cannot do so from "none of
    /// them, because one was wrong". AF-03 and AF-04 remain errors — neither
    /// is about an item.
    ///
    /// A request whose items are all rejected still succeeds, carrying a
    /// report that says so. The call did what it was asked: it reported.
    pub async fn add(
        &self,
        collection_uuid: Uuid,
        item_uuids: Vec<Uuid>,
        token: &str,
    ) -> Result<CollectionItemsResult, DomainError> {
        // AF-04: the caller must be authenticated. Evaluated before the
        // collection or items are looked up (FR-AU-07 / SRD §7).
        self.auth.authenticate(token).await?;

        // AF-03: the collection must exist. Still an error — a request naming
        // no collection has nothing to report per item.
        let collection = self
            .collection_repo
            .find_by_uuid(collection_uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        let mut items = Vec::with_capacity(item_uuids.len());

        for uuid in item_uuids {
            // Existence in the *other* table is what distinguishes AF-01 from
            // AF-02: the caller submits bare uuids with no declared kind, so a
            // uuid that resolves in the collection's own table is fine, one
            // that resolves only in the other table is a type mismatch, and
            // one that resolves in neither does not exist.
            let belongs = match collection.kind {
                CollectionKind::File => self.catalog_repo.find_by_uuid(uuid).await?.is_some(),
                CollectionKind::Bookmark => self.bookmark_repo.find_by_uuid(uuid).await?.is_some(),
            };

            if !belongs {
                let elsewhere = match collection.kind {
                    CollectionKind::File => self.bookmark_repo.find_by_uuid(uuid).await?.is_some(),
                    CollectionKind::Bookmark => {
                        self.catalog_repo.find_by_uuid(uuid).await?.is_some()
                    }
                };

                items.push(CollectionItemOutcome {
                    item_uuid: uuid,
                    added: false,
                    reason: Some(if elsewhere {
                        ItemRejection::WrongKind
                    } else {
                        ItemRejection::NotFound
                    }),
                });
                continue;
            }

            match collection.kind {
                CollectionKind::File => {
                    self.catalog_repo
                        .set_collection(uuid, collection_uuid)
                        .await?
                }
                CollectionKind::Bookmark => {
                    self.bookmark_repo
                        .set_collection(uuid, collection_uuid)
                        .await?
                }
            }

            items.push(CollectionItemOutcome {
                item_uuid: uuid,
                added: true,
                reason: None,
            });
        }

        Ok(CollectionItemsResult {
            collection_uuid,
            items,
        })
    }
}
