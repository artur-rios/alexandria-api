//! Unit tests for the UC-12 DeleteCollectionHandler (Testing Specification
//! §6). Each test exercises exactly the handler against trait fakes — no real
//! DB or auth service. Coverage follows §6.3: happy path, not-found (AF-01),
//! the unauthorized branch (AF-02), and the repository-write-failure branch.

use uuid::Uuid;

use alexandria_core::collections::commands::delete::DeleteCollectionHandler;
use alexandria_core::collections::model::{Collection, CollectionKind};
use alexandria_core::errors::DomainError;

use crate::common::{FakeAuth, FakeCollectionRepository};

const TOKEN: &str = "bearer-token";

fn handler(
    auth: FakeAuth,
    repo: FakeCollectionRepository,
) -> DeleteCollectionHandler<FakeAuth, FakeCollectionRepository> {
    DeleteCollectionHandler::new(auth, repo)
}

fn seeded() -> (
    FakeCollectionRepository,
    DeleteCollectionHandler<FakeAuth, FakeCollectionRepository>,
    Uuid,
) {
    let repo = FakeCollectionRepository::new();
    let uuid = Uuid::new_v4();
    repo.seed(Collection {
        uuid,
        name: "Sci-fi novels".to_string(),
        kind: CollectionKind::File,
    });
    let h = handler(FakeAuth::Allowing, repo.clone());
    (repo, h, uuid)
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_existing_collection_when_delete_then_removed_and_predelete_record_returned() {
    let (repo, h, uuid) = seeded();

    let result = h.delete(uuid, TOKEN).await.expect("delete");

    assert_eq!(result.uuid, uuid);
    assert_eq!(result.name, "Sci-fi novels");
    assert!(repo.collection_for(uuid).is_none(), "collection removed");
    assert_eq!(repo.count(), 0);
}

// ---------------- AF-01: not found ----------------

#[tokio::test]
async fn given_unknown_uuid_when_delete_then_not_found() {
    let repo = FakeCollectionRepository::new();
    let h = handler(FakeAuth::Allowing, repo.clone());

    let result = h.delete(Uuid::new_v4(), TOKEN).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

// ---------------- AF-02: unauthorized ----------------

#[tokio::test]
async fn given_unauthenticated_when_delete_then_unauthorized_and_collection_kept() {
    let repo = FakeCollectionRepository::new();
    let uuid = Uuid::new_v4();
    repo.seed(Collection {
        uuid,
        name: "Sci-fi novels".to_string(),
        kind: CollectionKind::File,
    });
    let h = handler(FakeAuth::Denying, repo.clone());

    let result = h.delete(uuid, "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
    assert!(repo.collection_for(uuid).is_some(), "collection kept");
}

#[tokio::test]
async fn given_unauthenticated_and_unknown_uuid_when_delete_then_unauthorized_not_not_found() {
    // Authentication is evaluated before the collection is looked up
    // (FR-AU-07 / SRD §7): an unauthenticated caller must not learn whether
    // the uuid exists.
    let repo = FakeCollectionRepository::new();
    let h = handler(FakeAuth::Denying, repo.clone());

    let result = h.delete(Uuid::new_v4(), "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

// ---------------- Repository write failure ----------------

#[tokio::test]
async fn given_delete_when_repo_write_fails_then_error_propagated_and_collection_kept() {
    let (repo, _h, uuid) = seeded();
    repo.fail_deletes();
    let h = handler(FakeAuth::Allowing, repo.clone());

    let result = h.delete(uuid, TOKEN).await;

    assert!(matches!(result, Err(DomainError::Internal(_))));
    assert!(repo.collection_for(uuid).is_some(), "collection kept");
}
