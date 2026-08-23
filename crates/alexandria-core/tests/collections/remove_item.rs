//! Unit tests for the UC-14 RemoveItemFromCollectionHandler (Testing
//! Specification §6). Each test exercises exactly the handler against trait
//! fakes — no real DB or auth service. Coverage follows §6.3: happy path for
//! each `kind`, AF-01 (item unknown or not in the collection), AF-02
//! (unknown collection), AF-03 (unauthorized).

use uuid::Uuid;

use alexandria_core::bookmarks::model::{Bookmark, BookmarkState};
use alexandria_core::catalog::model::{File, FileState, FileType};
use alexandria_core::catalog::repos::CatalogRepository;
use alexandria_core::collections::commands::remove_item::RemoveItemFromCollectionHandler;
use alexandria_core::collections::model::{Collection, CollectionKind};
use alexandria_core::errors::DomainError;

use crate::common::{
    FakeAuth, FakeBookmarkRepository, FakeCatalogRepository, FakeCollectionRepository,
};

const TOKEN: &str = "bearer-token";

type Handler = RemoveItemFromCollectionHandler<
    FakeAuth,
    FakeCollectionRepository,
    FakeCatalogRepository,
    FakeBookmarkRepository,
>;

fn handler(
    auth: FakeAuth,
    collection_repo: FakeCollectionRepository,
    catalog_repo: FakeCatalogRepository,
    bookmark_repo: FakeBookmarkRepository,
) -> Handler {
    RemoveItemFromCollectionHandler::new(auth, collection_repo, catalog_repo, bookmark_repo)
}

fn a_file(uuid: Uuid) -> File {
    File {
        uuid,
        path: format!("/lib/{uuid}.txt"),
        name: "note.txt".to_string(),
        file_type: FileType::Text,
        content_hash: Some("hash".to_string()),
        size_bytes: None,
        mtime: None,
        state: FileState::Active,
        deleted_at: None,
        indexed_at: chrono::Utc::now(),
        missing_at: None,
    }
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

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_linked_file_when_remove_then_unlinked_and_result_returned() {
    let collection_repo = FakeCollectionRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let collection_uuid = Uuid::new_v4();
    collection_repo.seed(Collection {
        uuid: collection_uuid,
        name: "My files".to_string(),
        kind: CollectionKind::File,
    });
    let file_uuid = Uuid::new_v4();
    catalog_repo.seed(a_file(file_uuid));
    catalog_repo
        .set_collection(file_uuid, collection_uuid)
        .await
        .expect("link");
    let h = handler(
        FakeAuth::Allowing,
        collection_repo,
        catalog_repo.clone(),
        FakeBookmarkRepository::new(),
    );

    let result = h
        .remove(collection_uuid, file_uuid, TOKEN)
        .await
        .expect("remove");

    assert_eq!(result.collection_uuid, collection_uuid);
    assert_eq!(result.item_uuid, file_uuid);
    assert_eq!(catalog_repo.collection_for_file(file_uuid), None);
}

#[tokio::test]
async fn given_linked_bookmark_when_remove_then_unlinked() {
    let collection_repo = FakeCollectionRepository::new();
    let bookmark_repo = FakeBookmarkRepository::new();
    let collection_uuid = Uuid::new_v4();
    collection_repo.seed(Collection {
        uuid: collection_uuid,
        name: "Reading list".to_string(),
        kind: CollectionKind::Bookmark,
    });
    let bookmark_uuid = Uuid::new_v4();
    bookmark_repo.seed(a_bookmark(bookmark_uuid, Some(collection_uuid)));
    let h = handler(
        FakeAuth::Allowing,
        collection_repo,
        FakeCatalogRepository::new(),
        bookmark_repo.clone(),
    );

    h.remove(collection_uuid, bookmark_uuid, TOKEN)
        .await
        .expect("remove");

    assert_eq!(
        bookmark_repo
            .bookmark_for(bookmark_uuid)
            .unwrap()
            .collection_uuid,
        None
    );
}

// ---------------- AF-01: item does not exist / not in the collection ----------------

#[tokio::test]
async fn given_unknown_item_when_remove_then_not_found() {
    let collection_repo = FakeCollectionRepository::new();
    let collection_uuid = Uuid::new_v4();
    collection_repo.seed(Collection {
        uuid: collection_uuid,
        name: "My files".to_string(),
        kind: CollectionKind::File,
    });
    let h = handler(
        FakeAuth::Allowing,
        collection_repo,
        FakeCatalogRepository::new(),
        FakeBookmarkRepository::new(),
    );

    let result = h.remove(collection_uuid, Uuid::new_v4(), TOKEN).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

#[tokio::test]
async fn given_item_not_in_this_collection_when_remove_then_not_found() {
    let collection_repo = FakeCollectionRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let collection_uuid = Uuid::new_v4();
    let other_collection_uuid = Uuid::new_v4();
    collection_repo.seed(Collection {
        uuid: collection_uuid,
        name: "My files".to_string(),
        kind: CollectionKind::File,
    });
    let file_uuid = Uuid::new_v4();
    catalog_repo.seed(a_file(file_uuid));
    catalog_repo
        .set_collection(file_uuid, other_collection_uuid)
        .await
        .expect("link elsewhere");
    let h = handler(
        FakeAuth::Allowing,
        collection_repo,
        catalog_repo.clone(),
        FakeBookmarkRepository::new(),
    );

    let result = h.remove(collection_uuid, file_uuid, TOKEN).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
    assert_eq!(
        catalog_repo.collection_for_file(file_uuid),
        Some(other_collection_uuid),
        "the item's actual link is untouched"
    );
}

// ---------------- AF-02: collection does not exist ----------------

#[tokio::test]
async fn given_unknown_collection_uuid_when_remove_then_not_found() {
    let h = handler(
        FakeAuth::Allowing,
        FakeCollectionRepository::new(),
        FakeCatalogRepository::new(),
        FakeBookmarkRepository::new(),
    );

    let result = h.remove(Uuid::new_v4(), Uuid::new_v4(), TOKEN).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

// ---------------- AF-03: unauthorized ----------------

#[tokio::test]
async fn given_unauthenticated_when_remove_then_unauthorized() {
    let h = handler(
        FakeAuth::Denying,
        FakeCollectionRepository::new(),
        FakeCatalogRepository::new(),
        FakeBookmarkRepository::new(),
    );

    let result = h.remove(Uuid::new_v4(), Uuid::new_v4(), "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}
