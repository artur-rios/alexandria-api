//! Unit tests for the UC-46 ListCollectionsHandler (Testing Specification
//! §6). Each test exercises exactly the handler against trait fakes — no real
//! DB or auth service. Coverage follows §6.3: the happy path with and without
//! a kind filter, AF-01 (nothing to list), AF-03 (unauthorized), and the
//! repository failing.
//!
//! AF-02 — an unrecognised `kind` — has no test here and cannot have one: the
//! parameter is the domain enum, so an unknown value has no way to reach this
//! handler. Both transports refuse it while parsing their own request, and
//! that is where it is tested (`collections_api.rs` and the FFI parity suite).

use uuid::Uuid;

use alexandria_core::collections::model::{Collection, CollectionKind};
use alexandria_core::collections::queries::list::ListCollectionsHandler;
use alexandria_core::errors::DomainError;

use crate::common::{FakeAuth, FakeCollectionRepository};

const TOKEN: &str = "bearer-token";

type Handler = ListCollectionsHandler<FakeAuth, FakeCollectionRepository>;

fn handler(auth: FakeAuth, collection_repo: FakeCollectionRepository) -> Handler {
    ListCollectionsHandler::new(auth, collection_repo)
}

fn a_collection(uuid: Uuid, name: &str, kind: CollectionKind) -> Collection {
    Collection {
        uuid,
        name: name.to_string(),
        kind,
    }
}

#[tokio::test]
async fn given_collections_of_both_kinds_when_list_without_filter_then_all_returned() {
    let repo = FakeCollectionRepository::new();
    let films = Uuid::new_v4();
    let reading = Uuid::new_v4();
    repo.seed(a_collection(films, "Films", CollectionKind::File));
    repo.seed(a_collection(reading, "Reading", CollectionKind::Bookmark));

    let result = handler(FakeAuth::Allowing, repo)
        .list(None, TOKEN)
        .await
        .unwrap();

    assert_eq!(result.len(), 2);
    assert_eq!(result[0].name, "Films");
    assert_eq!(result[1].name, "Reading");
}

#[tokio::test]
async fn given_collections_of_both_kinds_when_list_by_kind_then_only_that_kind_returned() {
    let repo = FakeCollectionRepository::new();
    repo.seed(a_collection(Uuid::new_v4(), "Films", CollectionKind::File));
    repo.seed(a_collection(
        Uuid::new_v4(),
        "Reading",
        CollectionKind::Bookmark,
    ));

    let result = handler(FakeAuth::Allowing, repo)
        .list(Some(CollectionKind::Bookmark), TOKEN)
        .await
        .unwrap();

    assert_eq!(result.len(), 1);
    assert_eq!(result[0].kind, CollectionKind::Bookmark);
    assert_eq!(result[0].name, "Reading");
}

#[tokio::test]
async fn given_a_collection_with_items_when_list_then_item_count_reported() {
    let repo = FakeCollectionRepository::new();
    let films = Uuid::new_v4();
    repo.seed(a_collection(films, "Films", CollectionKind::File));
    repo.set_item_count(films, 7);

    let result = handler(FakeAuth::Allowing, repo)
        .list(None, TOKEN)
        .await
        .unwrap();

    assert_eq!(result[0].item_count, 7);
}

#[tokio::test]
async fn given_an_empty_collection_when_list_then_counted_zero_and_still_listed() {
    let repo = FakeCollectionRepository::new();
    repo.seed(a_collection(Uuid::new_v4(), "Films", CollectionKind::File));

    let result = handler(FakeAuth::Allowing, repo)
        .list(None, TOKEN)
        .await
        .unwrap();

    assert_eq!(result.len(), 1);
    assert_eq!(result[0].item_count, 0);
}

/// AF-01: nothing to list is a state, not an error.
#[tokio::test]
async fn given_no_collections_when_list_then_empty_array_returned() {
    let result = handler(FakeAuth::Allowing, FakeCollectionRepository::new())
        .list(None, TOKEN)
        .await
        .unwrap();

    assert!(result.is_empty());
}

/// AF-01, reached the other way: collections exist, but none of the kind
/// asked for.
#[tokio::test]
async fn given_no_collection_of_that_kind_when_list_by_kind_then_empty_array_returned() {
    let repo = FakeCollectionRepository::new();
    repo.seed(a_collection(Uuid::new_v4(), "Films", CollectionKind::File));

    let result = handler(FakeAuth::Allowing, repo)
        .list(Some(CollectionKind::Bookmark), TOKEN)
        .await
        .unwrap();

    assert!(result.is_empty());
}

/// AF-03: the caller must be authenticated, and the repository is not read.
#[tokio::test]
async fn given_unauthenticated_when_list_then_unauthorized_and_repo_not_read() {
    let repo = FakeCollectionRepository::new();
    repo.seed(a_collection(Uuid::new_v4(), "Films", CollectionKind::File));
    // The read would succeed if it happened; failing it proves it did not.
    repo.fail_lists();

    let err = handler(FakeAuth::Denying, repo)
        .list(None, TOKEN)
        .await
        .unwrap_err();

    assert!(matches!(err, DomainError::Unauthorized));
}

#[tokio::test]
async fn given_list_when_repo_read_fails_then_error_propagated() {
    let repo = FakeCollectionRepository::new();
    repo.fail_lists();

    let err = handler(FakeAuth::Allowing, repo)
        .list(None, TOKEN)
        .await
        .unwrap_err();

    assert!(matches!(err, DomainError::Internal(_)));
}
