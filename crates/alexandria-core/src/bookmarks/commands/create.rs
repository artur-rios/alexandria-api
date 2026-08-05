use uuid::Uuid;

use crate::auth::AuthService;
use crate::bookmarks::model::{Bookmark, NewBookmark};
use crate::bookmarks::repos::BookmarkRepository;
use crate::collections::model::CollectionKind;
use crate::collections::repos::CollectionRepository;
use crate::errors::DomainError;

/// Validate a bookmark url (UC-15 / FR-BM-01, AF-01). The specification
/// requires only "a valid URL"; this project has no `url` crate dependency,
/// so validation is a minimal, dependency-free scheme check rather than a
/// full RFC 3986 parse — enough to reject garbage while accepting the
/// `scheme://host/path` shapes a browser bookmark actually has.
///
/// Rejects: empty; leading/trailing whitespace (silently trimming would store
/// a value different from what the caller sent); a NUL byte (would truncate
/// the string at the FFI boundary, desyncing the two transports); no
/// `scheme://` separator; an empty or non-alphanumeric scheme; nothing after
/// the separator.
pub fn validate_url(url: &str) -> Result<String, DomainError> {
    if url.is_empty() {
        return Err(DomainError::InvalidInput("bookmark url is required".into()));
    }
    if url != url.trim() {
        return Err(DomainError::InvalidInput(
            "bookmark url must not have leading or trailing whitespace".into(),
        ));
    }
    if url.as_bytes().contains(&0) {
        return Err(DomainError::InvalidInput(
            "bookmark url must not contain NUL".into(),
        ));
    }
    let scheme_end = url.find("://").ok_or_else(|| {
        DomainError::InvalidInput("bookmark url must include a scheme, e.g. https://".into())
    })?;
    let scheme = &url[..scheme_end];
    let rest = &url[scheme_end + 3..];
    let scheme_valid = !scheme.is_empty()
        && scheme
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.');
    if !scheme_valid {
        return Err(DomainError::InvalidInput(
            "bookmark url scheme is invalid".into(),
        ));
    }
    if rest.is_empty() {
        return Err(DomainError::InvalidInput(
            "bookmark url must have content after the scheme".into(),
        ));
    }
    Ok(url.to_string())
}

/// Validate a bookmark title (UC-15 / FR-BM-01, AF-01). Rejects: empty;
/// whitespace-only; leading/trailing whitespace; a NUL byte; longer than 255
/// bytes — the same rules `validate_collection_name` applies, for the same
/// reasons (NFR-09 parity).
pub fn validate_title(title: &str) -> Result<String, DomainError> {
    if title.is_empty() {
        return Err(DomainError::InvalidInput(
            "bookmark title is required".into(),
        ));
    }
    if title.trim().is_empty() {
        return Err(DomainError::InvalidInput(
            "bookmark title must not be blank".into(),
        ));
    }
    if title != title.trim() {
        return Err(DomainError::InvalidInput(
            "bookmark title must not have leading or trailing whitespace".into(),
        ));
    }
    if title.len() > 255 {
        return Err(DomainError::InvalidInput(
            "bookmark title is longer than 255 bytes".into(),
        ));
    }
    if title.as_bytes().contains(&0) {
        return Err(DomainError::InvalidInput(
            "bookmark title must not contain NUL".into(),
        ));
    }
    Ok(title.to_string())
}

/// UC-15 — Create a bookmark (FR-BM-01). Creates a browser bookmark,
/// optionally grouped into an existing bookmark collection, and returns the
/// record carrying its new public UUID.
///
/// Generic over two repositories: `BookmarkRepository` for the write, and
/// `CollectionRepository` to confirm — when a collection is referenced — that
/// it exists and is `kind = bookmark` (AF-02) before the write is attempted.
/// Like `CreateCollectionHandler` there is no `Clock` collaborator (a
/// bookmark's `deletedAt` is set only by UC-18) and no `Filesystem` (a
/// bookmark is catalog-only metadata with nothing on disk).
pub struct CreateBookmarkHandler<A, BR, CR> {
    auth: A,
    bookmark_repo: BR,
    collection_repo: CR,
}

impl<A, BR, CR> CreateBookmarkHandler<A, BR, CR>
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

    /// Create a bookmark with `url` and `title`, optionally in the bookmark
    /// collection identified by `collection_uuid`, and return the persisted
    /// record.
    pub async fn create(
        &self,
        url: &str,
        title: &str,
        collection_uuid: Option<Uuid>,
        token: &str,
    ) -> Result<Bookmark, DomainError> {
        // AF-03: the caller must be authenticated. Evaluated before any
        // payload is consulted (FR-AU-07 / SRD §7), so an unauthenticated
        // caller learns nothing about the request's validity.
        self.auth.authenticate(token).await?;

        // AF-01: the url and title must be valid.
        let url = validate_url(url)?;
        let title = validate_title(title)?;

        // AF-02: when a collection is referenced, it must exist and be a
        // bookmark collection. A referenced collection that does not exist
        // is `NotFound`, mirroring how every other use case treats a
        // dangling reference; a referenced collection of the wrong `kind` is
        // the invalid-input the specification names.
        if let Some(uuid) = collection_uuid {
            let collection = self
                .collection_repo
                .find_by_uuid(uuid)
                .await?
                .ok_or(DomainError::NotFound)?;
            if collection.kind != CollectionKind::Bookmark {
                return Err(DomainError::InvalidInput(
                    "referenced collection is not a bookmark collection".into(),
                ));
            }
        }

        self.bookmark_repo
            .insert_bookmark(NewBookmark {
                uuid: Uuid::new_v4(),
                url,
                title,
                collection_uuid,
            })
            .await
    }
}
