//! Unit tests for the UC-17 BrowseBookmarksHandler (Testing Specification
//! §6). Each test exercises exactly the handler against trait fakes — no
//! real DB or auth service. Coverage follows §6.3: happy path (unfiltered,
//! collection-filtered, state-filtered), the default-excludes-deleted
//! behavior required by the use case's main-flow step 2, AF-01 (unknown
//! collection), and AF-02 (unauthorized).

use uuid::Uuid;

use alexandria_core::bookmarks::model::{Bookmark, BookmarkState};
use alexandria_core::bookmarks::queries::browse::{BookmarkFilter, BrowseBookmarksHandler};
use alexandria_core::catalog::model::StateFilter;
use alexandria_core::collections::model::{Collection, CollectionKind};
use alexandria_core::errors::DomainError;

use crate::common::{FakeAuth, FakeBookmarkRepository, FakeCollectionRepository};

const TOKEN: &str = "bearer-token";

fn handler(
    auth: FakeAuth,
    bookmark_repo: FakeBookmarkRepository,
    collection_repo: FakeCollectionRepository,
) -> BrowseBookmarksHandler<FakeAuth, FakeBookmarkRepository, FakeCollectionRepository> {
    BrowseBookmarksHandler::new(auth, bookmark_repo, collection_repo)
}

fn a_bookmark(title: &str, collection_uuid: Option<Uuid>) -> Bookmark {
    Bookmark {
        uuid: Uuid::new_v4(),
        url: "https://example.com".to_string(),
        title: title.to_string(),
        state: BookmarkState::Active,
        deleted_at: None,
        collection_uuid,
    }
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_mixed_bookmarks_when_list_default_then_active_only_excludes_deleted() {
    let bookmark_repo = FakeBookmarkRepository::new();
    bookmark_repo.seed(a_bookmark("a", None));
    let mut deleted = a_bookmark("b", None);
    deleted.state = BookmarkState::Deleted;
    bookmark_repo.seed(deleted);
    let h = handler(
        FakeAuth::Allowing,
        bookmark_repo,
        FakeCollectionRepository::new(),
    );

    let bookmarks = h.list(BookmarkFilter::new(), TOKEN).await.expect("list");

    assert_eq!(bookmarks.len(), 1);
    assert_eq!(bookmarks[0].title, "a");
}

#[tokio::test]
async fn given_bookmarks_when_list_filtered_by_collection_then_only_linked_returned() {
    let bookmark_repo = FakeBookmarkRepository::new();
    let collection_repo = FakeCollectionRepository::new();
    let collection_uuid = Uuid::new_v4();
    collection_repo.seed(Collection {
        uuid: collection_uuid,
        name: "Reading list".to_string(),
        kind: CollectionKind::Bookmark,
    });
    bookmark_repo.seed(a_bookmark("a", Some(collection_uuid)));
    bookmark_repo.seed(a_bookmark("b", None));
    let h = handler(FakeAuth::Allowing, bookmark_repo, collection_repo);

    let filter = BookmarkFilter::new().with_collection(collection_uuid);
    let bookmarks = h.list(filter, TOKEN).await.expect("list");

    assert_eq!(bookmarks.len(), 1);
    assert_eq!(bookmarks[0].title, "a");
}

#[tokio::test]
async fn given_bookmarks_when_list_state_all_then_both_active_and_deleted_returned() {
    let bookmark_repo = FakeBookmarkRepository::new();
    bookmark_repo.seed(a_bookmark("a", None));
    let mut deleted = a_bookmark("b", None);
    deleted.state = BookmarkState::Deleted;
    bookmark_repo.seed(deleted);
    let h = handler(
        FakeAuth::Allowing,
        bookmark_repo,
        FakeCollectionRepository::new(),
    );

    let filter = BookmarkFilter::new().with_state(StateFilter::All);
    let bookmarks = h.list(filter, TOKEN).await.expect("list");

    assert_eq!(bookmarks.len(), 2);
}

#[tokio::test]
async fn given_no_bookmarks_when_list_then_empty_list_returned() {
    let h = handler(
        FakeAuth::Allowing,
        FakeBookmarkRepository::new(),
        FakeCollectionRepository::new(),
    );

    let bookmarks = h.list(BookmarkFilter::new(), TOKEN).await.expect("list");

    assert!(bookmarks.is_empty());
}

// ---------------- AF-01: referenced collection does not exist ----------------

#[tokio::test]
async fn given_unknown_collection_uuid_when_list_filtered_then_not_found() {
    let h = handler(
        FakeAuth::Allowing,
        FakeBookmarkRepository::new(),
        FakeCollectionRepository::new(),
    );

    let filter = BookmarkFilter::new().with_collection(Uuid::new_v4());
    let result = h.list(filter, TOKEN).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

// ---------------- AF-02: unauthorized ----------------

#[tokio::test]
async fn given_unauthenticated_when_list_then_unauthorized() {
    let h = handler(
        FakeAuth::Denying,
        FakeBookmarkRepository::new(),
        FakeCollectionRepository::new(),
    );

    let result = h.list(BookmarkFilter::new(), "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}
