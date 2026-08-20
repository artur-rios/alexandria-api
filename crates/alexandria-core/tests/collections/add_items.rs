//! Unit tests for the UC-13 AddItemsToCollectionHandler (Testing
//! Specification §6). Each test exercises exactly the handler against trait
//! fakes — no real DB or auth service. Coverage follows §6.3: happy path for
//! each `kind`, every AF: AF-01 (wrong-kind item), AF-02 (unknown item),
//! AF-03 (unknown collection), AF-04 (unauthorized).

use uuid::Uuid;

use alexandria_core::bookmarks::model::{Bookmark, BookmarkState};
use alexandria_core::catalog::model::{File, FileState, FileType};
use alexandria_core::collections::commands::add_items::AddItemsToCollectionHandler;
use alexandria_core::collections::model::{
    Collection, CollectionItemOutcome, CollectionItemsResult, CollectionKind, ItemRejection,
};
use alexandria_core::errors::DomainError;

use crate::common::{
    FakeAuth, FakeBookmarkRepository, FakeCatalogRepository, FakeCollectionRepository,
};

const TOKEN: &str = "bearer-token";

type Handler = AddItemsToCollectionHandler<
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
    AddItemsToCollectionHandler::new(auth, collection_repo, catalog_repo, bookmark_repo)
}

fn a_file(uuid: Uuid) -> File {
    File {
        uuid,
        path: format!("/lib/{uuid}.txt"),
        name: "note.txt".to_string(),
        file_type: FileType::Text,
        content_hash: "hash".to_string(),
        state: FileState::Active,
        deleted_at: None,
        indexed_at: chrono::Utc::now(),
        missing_at: None,
    }
}

fn a_bookmark(uuid: Uuid) -> Bookmark {
    Bookmark {
        uuid,
        url: "https://example.com".to_string(),
        title: "Example".to_string(),
        state: BookmarkState::Active,
        deleted_at: None,
        collection_uuid: None,
    }
}

/// The uuids the result reports as linked, in order.
fn added_uuids(result: &CollectionItemsResult) -> Vec<Uuid> {
    result
        .items
        .iter()
        .filter(|item| item.added)
        .map(|item| item.item_uuid)
        .collect()
}

/// What the result says about `uuid`, if it mentions it at all.
fn outcome_for(result: &CollectionItemsResult, uuid: Uuid) -> Option<&CollectionItemOutcome> {
    result.items.iter().find(|item| item.item_uuid == uuid)
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_file_collection_and_existing_files_when_add_then_linked_and_result_returned() {
    let collection_repo = FakeCollectionRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let collection_uuid = Uuid::new_v4();
    collection_repo.seed(Collection {
        uuid: collection_uuid,
        name: "My files".to_string(),
        kind: CollectionKind::File,
    });
    let file_a = Uuid::new_v4();
    let file_b = Uuid::new_v4();
    catalog_repo.seed(a_file(file_a));
    catalog_repo.seed(a_file(file_b));
    let h = handler(
        FakeAuth::Allowing,
        collection_repo,
        catalog_repo.clone(),
        FakeBookmarkRepository::new(),
    );

    let result = h
        .add(collection_uuid, vec![file_a, file_b], TOKEN)
        .await
        .expect("add");

    assert_eq!(result.collection_uuid, collection_uuid);
    assert_eq!(
        added_uuids(&result),
        vec![file_a, file_b],
        "both files were linked and reported"
    );
    assert_eq!(
        catalog_repo.collection_for_file(file_a),
        Some(collection_uuid)
    );
    assert_eq!(
        catalog_repo.collection_for_file(file_b),
        Some(collection_uuid)
    );
}

#[tokio::test]
async fn given_bookmark_collection_and_existing_bookmarks_when_add_then_linked() {
    let collection_repo = FakeCollectionRepository::new();
    let bookmark_repo = FakeBookmarkRepository::new();
    let collection_uuid = Uuid::new_v4();
    collection_repo.seed(Collection {
        uuid: collection_uuid,
        name: "Reading list".to_string(),
        kind: CollectionKind::Bookmark,
    });
    let bookmark_uuid = Uuid::new_v4();
    bookmark_repo.seed(a_bookmark(bookmark_uuid));
    let h = handler(
        FakeAuth::Allowing,
        collection_repo,
        FakeCatalogRepository::new(),
        bookmark_repo.clone(),
    );

    let result = h
        .add(collection_uuid, vec![bookmark_uuid], TOKEN)
        .await
        .expect("add");

    assert_eq!(added_uuids(&result), vec![bookmark_uuid]);
    assert_eq!(
        bookmark_repo
            .bookmark_for(bookmark_uuid)
            .unwrap()
            .collection_uuid,
        Some(collection_uuid)
    );
}

// ---------------- AF-01: item type does not match collection kind ----------------

#[tokio::test]
async fn given_bookmark_item_for_file_collection_when_add_then_invalid_input_and_nothing_linked() {
    let collection_repo = FakeCollectionRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let bookmark_repo = FakeBookmarkRepository::new();
    let collection_uuid = Uuid::new_v4();
    collection_repo.seed(Collection {
        uuid: collection_uuid,
        name: "My files".to_string(),
        kind: CollectionKind::File,
    });
    let bookmark_uuid = Uuid::new_v4();
    bookmark_repo.seed(a_bookmark(bookmark_uuid));
    let h = handler(
        FakeAuth::Allowing,
        collection_repo,
        catalog_repo.clone(),
        bookmark_repo,
    );

    let result = h
        .add(collection_uuid, vec![bookmark_uuid], TOKEN)
        .await
        .expect("the request succeeds; the item is what was rejected");

    let outcome = outcome_for(&result, bookmark_uuid).expect("reported");
    assert!(!outcome.added);
    assert_eq!(outcome.reason, Some(ItemRejection::WrongKind));
    assert_eq!(catalog_repo.collection_for_file(bookmark_uuid), None);
}

#[tokio::test]
async fn given_file_item_for_bookmark_collection_when_add_then_invalid_input() {
    let collection_repo = FakeCollectionRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let bookmark_repo = FakeBookmarkRepository::new();
    let collection_uuid = Uuid::new_v4();
    collection_repo.seed(Collection {
        uuid: collection_uuid,
        name: "Reading list".to_string(),
        kind: CollectionKind::Bookmark,
    });
    let file_uuid = Uuid::new_v4();
    catalog_repo.seed(a_file(file_uuid));
    let h = handler(
        FakeAuth::Allowing,
        collection_repo,
        catalog_repo,
        bookmark_repo,
    );

    let result = h
        .add(collection_uuid, vec![file_uuid], TOKEN)
        .await
        .expect("the request succeeds; the item is what was rejected");

    let outcome = outcome_for(&result, file_uuid).expect("reported");
    assert!(!outcome.added);
    assert_eq!(outcome.reason, Some(ItemRejection::WrongKind));
}

#[tokio::test]
async fn given_one_valid_and_one_wrong_kind_item_when_add_then_the_valid_one_is_linked() {
    // AF-01: the wrong-kind item is reported and the valid one still lands.
    // This is the behaviour the per-item report exists for — a caller can
    // tell its owner exactly what happened, which "none of them" never
    // allowed.
    let collection_repo = FakeCollectionRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let bookmark_repo = FakeBookmarkRepository::new();
    let collection_uuid = Uuid::new_v4();
    collection_repo.seed(Collection {
        uuid: collection_uuid,
        name: "My files".to_string(),
        kind: CollectionKind::File,
    });
    let good_file = Uuid::new_v4();
    catalog_repo.seed(a_file(good_file));
    let bad_bookmark = Uuid::new_v4();
    bookmark_repo.seed(a_bookmark(bad_bookmark));
    let h = handler(
        FakeAuth::Allowing,
        collection_repo,
        catalog_repo.clone(),
        bookmark_repo,
    );

    let result = h
        .add(collection_uuid, vec![good_file, bad_bookmark], TOKEN)
        .await
        .expect("add");

    assert_eq!(added_uuids(&result), vec![good_file]);
    assert_eq!(
        outcome_for(&result, bad_bookmark).and_then(|item| item.reason),
        Some(ItemRejection::WrongKind)
    );
    assert_eq!(
        catalog_repo.collection_for_file(good_file),
        Some(collection_uuid),
        "one bad item no longer costs the good ones"
    );
}

// ---------------- AF-02: referenced item does not exist ----------------

#[tokio::test]
async fn given_unknown_item_uuid_when_add_then_reported_as_not_found() {
    let collection_repo = FakeCollectionRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let collection_uuid = Uuid::new_v4();
    collection_repo.seed(Collection {
        uuid: collection_uuid,
        name: "My files".to_string(),
        kind: CollectionKind::File,
    });
    let h = handler(
        FakeAuth::Allowing,
        collection_repo,
        catalog_repo.clone(),
        FakeBookmarkRepository::new(),
    );

    let unknown = Uuid::new_v4();
    let result = h
        .add(collection_uuid, vec![unknown], TOKEN)
        .await
        .expect("the request succeeds; the item is what was rejected");

    let outcome = outcome_for(&result, unknown).expect("reported");
    assert!(!outcome.added);
    assert_eq!(
        outcome.reason,
        Some(ItemRejection::NotFound),
        "told apart from the wrong-kind rejection: this uuid names nothing at all"
    );
    assert_eq!(catalog_repo.collection_for_file(unknown), None);
}

/// AF-05: nothing was linked, and the caller is told why for each. A request
/// that reported nothing would be indistinguishable from one that worked.
#[tokio::test]
async fn given_every_item_rejected_when_add_then_it_succeeds_with_a_full_report() {
    let collection_repo = FakeCollectionRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let bookmark_repo = FakeBookmarkRepository::new();
    let collection_uuid = Uuid::new_v4();
    collection_repo.seed(Collection {
        uuid: collection_uuid,
        name: "My files".to_string(),
        kind: CollectionKind::File,
    });
    let wrong_kind = Uuid::new_v4();
    bookmark_repo.seed(a_bookmark(wrong_kind));
    let unknown = Uuid::new_v4();
    let h = handler(
        FakeAuth::Allowing,
        collection_repo,
        catalog_repo,
        bookmark_repo,
    );

    let result = h
        .add(collection_uuid, vec![wrong_kind, unknown], TOKEN)
        .await
        .expect("reporting is what the call was asked for");

    assert!(added_uuids(&result).is_empty());
    assert_eq!(result.items.len(), 2);
    assert_eq!(
        outcome_for(&result, wrong_kind).and_then(|item| item.reason),
        Some(ItemRejection::WrongKind)
    );
    assert_eq!(
        outcome_for(&result, unknown).and_then(|item| item.reason),
        Some(ItemRejection::NotFound)
    );
}

// ---------------- AF-03: collection does not exist ----------------

#[tokio::test]
async fn given_unknown_collection_uuid_when_add_then_not_found() {
    let h = handler(
        FakeAuth::Allowing,
        FakeCollectionRepository::new(),
        FakeCatalogRepository::new(),
        FakeBookmarkRepository::new(),
    );

    let result = h.add(Uuid::new_v4(), vec![Uuid::new_v4()], TOKEN).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

// ---------------- AF-04: unauthorized ----------------

#[tokio::test]
async fn given_unauthenticated_when_add_then_unauthorized() {
    let h = handler(
        FakeAuth::Denying,
        FakeCollectionRepository::new(),
        FakeCatalogRepository::new(),
        FakeBookmarkRepository::new(),
    );

    let result = h.add(Uuid::new_v4(), vec![Uuid::new_v4()], "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

#[tokio::test]
async fn given_unauthenticated_and_unknown_collection_when_add_then_unauthorized_not_not_found() {
    // Authentication is evaluated before the collection is looked up
    // (FR-AU-07 / SRD §7): an unauthenticated caller must not learn whether
    // the collection exists.
    let h = handler(
        FakeAuth::Denying,
        FakeCollectionRepository::new(),
        FakeCatalogRepository::new(),
        FakeBookmarkRepository::new(),
    );

    let result = h.add(Uuid::new_v4(), vec![Uuid::new_v4()], "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}
