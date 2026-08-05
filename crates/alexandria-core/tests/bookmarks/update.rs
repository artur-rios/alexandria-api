//! Unit tests for the UC-16 UpdateBookmarkHandler (Testing Specification
//! §6). Each test exercises exactly the handler against trait fakes — no
//! real DB or auth service. Coverage follows §6.3: happy path (with and
//! without a collection, and clearing an existing one), every url/title
//! validation failure (AF-01), the wrong-kind/missing collection branch
//! (AF-02-style), not-found (AF-02), the deleted-state precondition, the
//! unauthorized branch (AF-03), and the repository-write-failure branch.

use uuid::Uuid;

use alexandria_core::bookmarks::commands::update::UpdateBookmarkHandler;
use alexandria_core::bookmarks::model::{Bookmark, BookmarkState};
use alexandria_core::collections::model::{Collection, CollectionKind};
use alexandria_core::errors::DomainError;

use crate::common::{FakeAuth, FakeBookmarkRepository, FakeCollectionRepository};

const TOKEN: &str = "bearer-token";

fn handler(
    auth: FakeAuth,
    bookmark_repo: FakeBookmarkRepository,
    collection_repo: FakeCollectionRepository,
) -> UpdateBookmarkHandler<FakeAuth, FakeBookmarkRepository, FakeCollectionRepository> {
    UpdateBookmarkHandler::new(auth, bookmark_repo, collection_repo)
}

fn a_bookmark(uuid: Uuid, collection_uuid: Option<Uuid>) -> Bookmark {
    Bookmark {
        uuid,
        url: "https://example.com".to_string(),
        title: "Example".to_string(),
        state: BookmarkState::Active,
        deleted_at: None,
        collection_uuid,
    }
}

fn seeded() -> (
    FakeBookmarkRepository,
    UpdateBookmarkHandler<FakeAuth, FakeBookmarkRepository, FakeCollectionRepository>,
    Uuid,
) {
    let bookmark_repo = FakeBookmarkRepository::new();
    let uuid = Uuid::new_v4();
    bookmark_repo.seed(a_bookmark(uuid, None));
    let h = handler(
        FakeAuth::Allowing,
        bookmark_repo.clone(),
        FakeCollectionRepository::new(),
    );
    (bookmark_repo, h, uuid)
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_valid_fields_when_update_then_bookmark_updated_and_returned() {
    let (repo, h, uuid) = seeded();

    let result = h
        .update(uuid, "https://example.org", "New title", None, TOKEN)
        .await
        .expect("update");

    assert_eq!(result.url, "https://example.org");
    assert_eq!(result.title, "New title");
    assert_eq!(repo.bookmark_for(uuid).unwrap().url, "https://example.org");
}

#[tokio::test]
async fn given_bookmark_collection_when_update_then_linked() {
    let bookmark_repo = FakeBookmarkRepository::new();
    let collection_repo = FakeCollectionRepository::new();
    let uuid = Uuid::new_v4();
    bookmark_repo.seed(a_bookmark(uuid, None));
    let collection_uuid = Uuid::new_v4();
    collection_repo.seed(Collection {
        uuid: collection_uuid,
        name: "Reading list".to_string(),
        kind: CollectionKind::Bookmark,
    });
    let h = handler(FakeAuth::Allowing, bookmark_repo.clone(), collection_repo);

    let result = h
        .update(
            uuid,
            "https://example.com",
            "Example",
            Some(collection_uuid),
            TOKEN,
        )
        .await
        .expect("update");

    assert_eq!(result.collection_uuid, Some(collection_uuid));
}

#[tokio::test]
async fn given_previously_linked_bookmark_and_no_collection_when_update_then_unlinked() {
    let bookmark_repo = FakeBookmarkRepository::new();
    let uuid = Uuid::new_v4();
    let old_collection = Uuid::new_v4();
    bookmark_repo.seed(a_bookmark(uuid, Some(old_collection)));
    let h = handler(
        FakeAuth::Allowing,
        bookmark_repo.clone(),
        FakeCollectionRepository::new(),
    );

    let result = h
        .update(uuid, "https://example.com", "Example", None, TOKEN)
        .await
        .expect("update");

    assert_eq!(result.collection_uuid, None, "full replace clears the link");
}

// ---------------- AF-01: invalid input (url / title) ----------------

#[tokio::test]
async fn given_empty_url_when_update_then_invalid_input_and_bookmark_unchanged() {
    let (repo, h, uuid) = seeded();

    let result = h.update(uuid, "", "New title", None, TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert_eq!(repo.bookmark_for(uuid).unwrap().url, "https://example.com");
}

#[tokio::test]
async fn given_empty_title_when_update_then_invalid_input() {
    let (repo, h, uuid) = seeded();

    let result = h.update(uuid, "https://example.org", "", None, TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert_eq!(repo.bookmark_for(uuid).unwrap().title, "Example");
}

// ---------------- Referenced collection ----------------

#[tokio::test]
async fn given_collection_of_wrong_kind_when_update_then_invalid_input_and_bookmark_unchanged() {
    let bookmark_repo = FakeBookmarkRepository::new();
    let collection_repo = FakeCollectionRepository::new();
    let uuid = Uuid::new_v4();
    bookmark_repo.seed(a_bookmark(uuid, None));
    let collection_uuid = Uuid::new_v4();
    collection_repo.seed(Collection {
        uuid: collection_uuid,
        name: "My files".to_string(),
        kind: CollectionKind::File,
    });
    let h = handler(FakeAuth::Allowing, bookmark_repo.clone(), collection_repo);

    let result = h
        .update(
            uuid,
            "https://example.com",
            "Example",
            Some(collection_uuid),
            TOKEN,
        )
        .await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert_eq!(
        bookmark_repo.bookmark_for(uuid).unwrap().collection_uuid,
        None
    );
}

#[tokio::test]
async fn given_unknown_collection_uuid_when_update_then_not_found() {
    let (repo, h, uuid) = seeded();

    let result = h
        .update(
            uuid,
            "https://example.com",
            "Example",
            Some(Uuid::new_v4()),
            TOKEN,
        )
        .await;

    assert!(matches!(result, Err(DomainError::NotFound)));
    assert_eq!(repo.bookmark_for(uuid).unwrap().collection_uuid, None);
}

// ---------------- AF-02: bookmark does not exist ----------------

#[tokio::test]
async fn given_unknown_bookmark_uuid_when_update_then_not_found() {
    let h = handler(
        FakeAuth::Allowing,
        FakeBookmarkRepository::new(),
        FakeCollectionRepository::new(),
    );

    let result = h
        .update(
            Uuid::new_v4(),
            "https://example.com",
            "Example",
            None,
            TOKEN,
        )
        .await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

// ---------------- Precondition: bookmark must be active ----------------

#[tokio::test]
async fn given_deleted_bookmark_when_update_then_invalid_state() {
    let bookmark_repo = FakeBookmarkRepository::new();
    let uuid = Uuid::new_v4();
    let mut bookmark = a_bookmark(uuid, None);
    bookmark.state = BookmarkState::Deleted;
    bookmark_repo.seed(bookmark);
    let h = handler(
        FakeAuth::Allowing,
        bookmark_repo,
        FakeCollectionRepository::new(),
    );

    let result = h
        .update(uuid, "https://example.org", "New title", None, TOKEN)
        .await;

    assert!(matches!(result, Err(DomainError::InvalidState)));
}

// ---------------- AF-03: unauthorized ----------------

#[tokio::test]
async fn given_unauthenticated_when_update_then_unauthorized_and_bookmark_unchanged() {
    let bookmark_repo = FakeBookmarkRepository::new();
    let uuid = Uuid::new_v4();
    bookmark_repo.seed(a_bookmark(uuid, None));
    let h = handler(
        FakeAuth::Denying,
        bookmark_repo.clone(),
        FakeCollectionRepository::new(),
    );

    let result = h
        .update(uuid, "https://example.org", "New title", None, "")
        .await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
    assert_eq!(
        bookmark_repo.bookmark_for(uuid).unwrap().url,
        "https://example.com"
    );
}

#[tokio::test]
async fn given_unauthenticated_and_unknown_uuid_when_update_then_unauthorized_not_not_found() {
    let h = handler(
        FakeAuth::Denying,
        FakeBookmarkRepository::new(),
        FakeCollectionRepository::new(),
    );

    let result = h
        .update(Uuid::new_v4(), "https://example.com", "Example", None, "")
        .await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

// ---------------- Repository write failure ----------------

#[tokio::test]
async fn given_update_when_repo_write_fails_then_error_propagated_and_bookmark_unchanged() {
    let (repo, _h, uuid) = seeded();
    repo.fail_updates();
    let h = handler(
        FakeAuth::Allowing,
        repo.clone(),
        FakeCollectionRepository::new(),
    );

    let result = h
        .update(uuid, "https://example.org", "New title", None, TOKEN)
        .await;

    assert!(matches!(result, Err(DomainError::Internal(_))));
    assert_eq!(repo.bookmark_for(uuid).unwrap().url, "https://example.com");
}
