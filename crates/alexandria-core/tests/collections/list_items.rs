//! Unit tests for the UC-14 ListCollectionItemsHandler (Testing
//! Specification §6). Each test exercises exactly the handler against trait
//! fakes — no real DB or auth service. Coverage follows §6.3: happy path for
//! each `kind` (including empty membership), AF-01 (unknown collection),
//! AF-02 (unauthorized).

use uuid::Uuid;

use alexandria_core::bookmarks::model::{Bookmark, BookmarkState};
use alexandria_core::catalog::model::{File, FileState, FileType};
use alexandria_core::catalog::repos::CatalogRepository;
use alexandria_core::collections::model::{Collection, CollectionItems, CollectionKind};
use alexandria_core::collections::queries::list_items::ListCollectionItemsHandler;
use alexandria_core::errors::DomainError;

use crate::common::{
    FakeAuth, FakeBookmarkRepository, FakeCatalogRepository, FakeCollectionRepository,
};

const TOKEN: &str = "bearer-token";

type Handler = ListCollectionItemsHandler<
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
    ListCollectionItemsHandler::new(auth, collection_repo, catalog_repo, bookmark_repo)
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
        metadata_version: 0,
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
async fn given_file_collection_with_members_when_list_then_files_returned() {
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
        catalog_repo,
        FakeBookmarkRepository::new(),
    );

    let result = h.list(collection_uuid, TOKEN).await.expect("list");

    assert_eq!(result.collection_uuid, collection_uuid);
    assert_eq!(result.kind, CollectionKind::File);
    match result.items {
        CollectionItems::Files(files) => {
            assert_eq!(files.len(), 1);
            assert_eq!(files[0].uuid, file_uuid);
        }
        CollectionItems::Bookmarks(_) => panic!("expected files"),
    }
}

#[tokio::test]
async fn given_bookmark_collection_with_members_when_list_then_bookmarks_returned() {
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
        bookmark_repo,
    );

    let result = h.list(collection_uuid, TOKEN).await.expect("list");

    assert_eq!(result.kind, CollectionKind::Bookmark);
    match result.items {
        CollectionItems::Bookmarks(bookmarks) => {
            assert_eq!(bookmarks.len(), 1);
            assert_eq!(bookmarks[0].uuid, bookmark_uuid);
        }
        CollectionItems::Files(_) => panic!("expected bookmarks"),
    }
}

#[tokio::test]
async fn given_empty_collection_when_list_then_empty_array_returned() {
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

    let result = h.list(collection_uuid, TOKEN).await.expect("list");

    match result.items {
        CollectionItems::Files(files) => assert!(files.is_empty()),
        CollectionItems::Bookmarks(_) => panic!("expected files"),
    }
}

// ---------------- AF-01: collection does not exist ----------------

#[tokio::test]
async fn given_unknown_collection_uuid_when_list_then_not_found() {
    let h = handler(
        FakeAuth::Allowing,
        FakeCollectionRepository::new(),
        FakeCatalogRepository::new(),
        FakeBookmarkRepository::new(),
    );

    let result = h.list(Uuid::new_v4(), TOKEN).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

// ---------------- AF-02: unauthorized ----------------

#[tokio::test]
async fn given_unauthenticated_when_list_then_unauthorized() {
    let h = handler(
        FakeAuth::Denying,
        FakeCollectionRepository::new(),
        FakeCatalogRepository::new(),
        FakeBookmarkRepository::new(),
    );

    let result = h.list(Uuid::new_v4(), "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}
