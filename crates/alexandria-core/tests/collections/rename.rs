//! Unit tests for the UC-11 RenameCollectionHandler (Testing Specification
//! §6). Each test exercises exactly the handler against trait fakes — no real
//! DB or auth service. Coverage follows §6.3: happy path, name-validation
//! failures (AF-01), not-found (AF-02), the unauthorized branch (AF-03), and
//! the repository-write-failure branch.

use uuid::Uuid;

use alexandria_core::collections::commands::rename::RenameCollectionHandler;
use alexandria_core::collections::model::{Collection, CollectionKind};
use alexandria_core::errors::DomainError;

use crate::common::{FakeAuth, FakeCollectionRepository};

const TOKEN: &str = "bearer-token";

fn handler(
    auth: FakeAuth,
    repo: FakeCollectionRepository,
) -> RenameCollectionHandler<FakeAuth, FakeCollectionRepository> {
    RenameCollectionHandler::new(auth, repo)
}

/// An allowing handler over a fake repo seeded with one collection, plus the
/// repo and the seeded collection's uuid.
fn seeded() -> (
    FakeCollectionRepository,
    RenameCollectionHandler<FakeAuth, FakeCollectionRepository>,
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
async fn given_existing_collection_and_valid_name_when_rename_then_name_updated_and_returned() {
    let (repo, h, uuid) = seeded();

    let result = h
        .rename(uuid, "Sci-fi & fantasy", TOKEN)
        .await
        .expect("rename");

    assert_eq!(result.name, "Sci-fi & fantasy");
    assert_eq!(result.uuid, uuid, "uuid is unchanged");
    assert_eq!(result.kind, CollectionKind::File, "kind is unchanged");
    assert_eq!(repo.collection_for(uuid).unwrap().name, "Sci-fi & fantasy");
}

// ---------------- AF-01: invalid input (name) ----------------

#[tokio::test]
async fn given_empty_name_when_rename_then_invalid_input_and_name_unchanged() {
    let (repo, h, uuid) = seeded();

    let result = h.rename(uuid, "", TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert_eq!(repo.collection_for(uuid).unwrap().name, "Sci-fi novels");
}

#[tokio::test]
async fn given_whitespace_only_name_when_rename_then_invalid_input() {
    let (repo, h, uuid) = seeded();

    let result = h.rename(uuid, "   ", TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert_eq!(repo.collection_for(uuid).unwrap().name, "Sci-fi novels");
}

#[tokio::test]
async fn given_untrimmed_name_when_rename_then_invalid_input_rather_than_silent_trim() {
    let (repo, h, uuid) = seeded();

    let result = h.rename(uuid, "  Favorites  ", TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert_eq!(repo.collection_for(uuid).unwrap().name, "Sci-fi novels");
}

// ---------------- AF-02: not found ----------------

#[tokio::test]
async fn given_unknown_uuid_when_rename_then_not_found() {
    let repo = FakeCollectionRepository::new();
    let h = handler(FakeAuth::Allowing, repo.clone());

    let result = h.rename(Uuid::new_v4(), "New name", TOKEN).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

// ---------------- AF-03: unauthorized ----------------

#[tokio::test]
async fn given_unauthenticated_when_rename_then_unauthorized_and_name_unchanged() {
    let repo = FakeCollectionRepository::new();
    let uuid = Uuid::new_v4();
    repo.seed(Collection {
        uuid,
        name: "Sci-fi novels".to_string(),
        kind: CollectionKind::File,
    });
    let h = handler(FakeAuth::Denying, repo.clone());

    let result = h.rename(uuid, "New name", "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
    assert_eq!(repo.collection_for(uuid).unwrap().name, "Sci-fi novels");
}

#[tokio::test]
async fn given_unauthenticated_and_unknown_uuid_when_rename_then_unauthorized_not_not_found() {
    // Authentication is evaluated before the collection is looked up
    // (FR-AU-07 / SRD §7): an unauthenticated caller must not learn whether
    // the uuid exists.
    let repo = FakeCollectionRepository::new();
    let h = handler(FakeAuth::Denying, repo.clone());

    let result = h.rename(Uuid::new_v4(), "New name", "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

// ---------------- Repository write failure ----------------

#[tokio::test]
async fn given_rename_when_repo_write_fails_then_error_propagated_and_name_unchanged() {
    let (repo, _h, uuid) = seeded();
    repo.fail_renames();
    let h = handler(FakeAuth::Allowing, repo.clone());

    let result = h.rename(uuid, "New name", TOKEN).await;

    assert!(matches!(result, Err(DomainError::Internal(_))));
    assert_eq!(repo.collection_for(uuid).unwrap().name, "Sci-fi novels");
}
