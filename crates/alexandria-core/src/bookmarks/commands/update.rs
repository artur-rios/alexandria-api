use uuid::Uuid;

use crate::auth::AuthService;
use crate::bookmarks::commands::create::{validate_title, validate_url};
use crate::bookmarks::model::{Bookmark, BookmarkState};
use crate::bookmarks::repos::BookmarkRepository;
use crate::collections::model::CollectionKind;
use crate::collections::repos::CollectionRepository;
use crate::errors::DomainError;

/// UC-16 — Update a bookmark (FR-BM-02). Replaces a bookmark's url, title,
/// and containing collection.
///
/// Full replace, not a merge, matching how `EditMetadataHandler` (UC-04)
/// treats a file's subtype fields: the caller resubmits every field, and
/// `collection_uuid = None` clears the link rather than leaving it
/// untouched. Generic over the same two repositories `CreateBookmarkHandler`
/// uses, for the same reason: a referenced collection must exist and be
/// `kind = bookmark` before the write is attempted.
pub struct UpdateBookmarkHandler<A, BR, CR> {
    auth: A,
    bookmark_repo: BR,
    collection_repo: CR,
}

impl<A, BR, CR> UpdateBookmarkHandler<A, BR, CR>
where
    A: AuthService,
    BR: BookmarkRepository,
    CR: CollectionRepository,
{
    pub fn new(auth: A, bookmark_repo: BR, collection_repo: CR) -> Self {
        Self {
            auth,
            bookmark_repo,
            collection_repo,
        }
    }

    /// Replace the bookmark identified by `uuid` with `url`, `title`, and
    /// `collection_uuid`, and return the updated record.
    pub async fn update(
        &self,
        uuid: Uuid,
        url: &str,
        title: &str,
        collection_uuid: Option<Uuid>,
        token: &str,
    ) -> Result<Bookmark, DomainError> {
        // AF-03: the caller must be authenticated. Evaluated before the
        // bookmark is looked up or the payload validated (FR-AU-07 / SRD
        // §7).
        self.auth.authenticate(token).await?;

        // AF-02: the bookmark must exist.
        let bookmark = self
            .bookmark_repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        // Precondition: the bookmark must be active — restore via UC-18
        // before updating a soft-deleted one, matching how UC-04/UC-05 guard
        // their own "active" precondition even without a dedicated AF row.
        if bookmark.state == BookmarkState::Deleted {
            return Err(DomainError::InvalidState);
        }

        // AF-01: the url and title must be valid.
        let url = validate_url(url)?;
        let title = validate_title(title)?;

        // Same collection check UC-15 applies at creation: a referenced
        // collection must exist and be a bookmark collection, so update
        // cannot be used to violate the invariant create enforces.
        if let Some(cu) = collection_uuid {
            let collection = self
                .collection_repo
                .find_by_uuid(cu)
                .await?
                .ok_or(DomainError::NotFound)?;
            if collection.kind != CollectionKind::Bookmark {
                return Err(DomainError::InvalidInput(
                    "referenced collection is not a bookmark collection".into(),
                ));
            }
        }

        self.bookmark_repo
            .update_bookmark(uuid, url, title, collection_uuid)
            .await
    }
}
