//! Unit tests for the UC-10 CreateCollectionHandler (Testing Specification
//! §6). Each test exercises exactly the handler against trait fakes — no real
//! DB or auth service. Coverage follows §6.3: happy path for each `kind`,
//! every name-validation failure (AF-01), the unauthorized branch (AF-02),
//! and the repository-write-failure branch.
//!
//! There is no not-found or invalid-state case: UC-10 creates a new record, so
//! it references no existing entity and the collection has no lifecycle.

use alexandria_core::collections::commands::create::CreateCollectionHandler;
use alexandria_core::collections::model::CollectionKind;
use alexandria_core::errors::DomainError;

use crate::common::{FakeAuth, FakeCollectionRepository};

const TOKEN: &str = "bearer-token";

fn handler(
    auth: FakeAuth,
    repo: FakeCollectionRepository,
) -> CreateCollectionHandler<FakeAuth, FakeCollectionRepository> {
    CreateCollectionHandler::new(auth, repo)
}

/// An allowing handler over a fresh fake repo, plus the repo to inspect.
fn seeded() -> (
    FakeCollectionRepository,
    CreateCollectionHandler<FakeAuth, FakeCollectionRepository>,
) {
    let repo = FakeCollectionRepository::new();
    let h = handler(FakeAuth::Allowing, repo.clone());
    (repo, h)
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_valid_name_and_file_kind_when_create_then_collection_persisted_and_returned() {
    let (repo, h) = seeded();

    let result = h
        .create("Sci-fi novels", CollectionKind::File, TOKEN)
        .await
        .expect("create");

    assert_eq!(result.name, "Sci-fi novels");
    assert_eq!(result.kind, CollectionKind::File);
    assert!(!result.uuid.is_nil(), "a uuid was minted");

    let persisted = repo.collection_for(result.uuid).expect("persisted");
    assert_eq!(persisted, result, "the returned record is the stored one");
    assert_eq!(repo.count(), 1);
}

#[tokio::test]
async fn given_valid_name_and_bookmark_kind_when_create_then_kind_persisted_as_bookmark() {
    let (repo, h) = seeded();

    let result = h
        .create("Rust reading", CollectionKind::Bookmark, TOKEN)
        .await
        .expect("create");

    assert_eq!(result.kind, CollectionKind::Bookmark);
    assert_eq!(
        repo.collection_for(result.uuid).unwrap().kind,
        CollectionKind::Bookmark
    );
}

#[tokio::test]
async fn given_two_collections_with_same_name_when_create_then_both_get_distinct_uuids() {
    // Nothing in the specification makes a collection name unique, so the same
    // name twice is two collections — distinguished by their public uuid.
    let (repo, h) = seeded();

    let first = h
        .create("Favorites", CollectionKind::File, TOKEN)
        .await
        .expect("first");
    let second = h
        .create("Favorites", CollectionKind::File, TOKEN)
        .await
        .expect("second");

    assert_ne!(first.uuid, second.uuid);
    assert_eq!(repo.count(), 2);
}

// ---------------- AF-01: invalid input (name) ----------------

#[tokio::test]
async fn given_empty_name_when_create_then_invalid_input_and_nothing_persisted() {
    let (repo, h) = seeded();

    let result = h.create("", CollectionKind::File, TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert_eq!(repo.count(), 0);
}

#[tokio::test]
async fn given_whitespace_only_name_when_create_then_invalid_input_and_nothing_persisted() {
    let (repo, h) = seeded();

    let result = h.create("   ", CollectionKind::File, TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert_eq!(repo.count(), 0);
}

#[tokio::test]
async fn given_untrimmed_name_when_create_then_invalid_input_rather_than_silent_trim() {
    // Trimming for the caller would store a name different from the one they
    // sent, and would do so identically on neither surface by accident — the
    // rejection is what keeps HTTP and FFI agreeing (FR-FC-24 / NFR-09).
    let (repo, h) = seeded();

    let result = h.create("  Favorites  ", CollectionKind::File, TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert_eq!(repo.count(), 0);
}

#[tokio::test]
async fn given_name_longer_than_255_bytes_when_create_then_invalid_input() {
    let (repo, h) = seeded();
    let long = "n".repeat(256);

    let result = h.create(&long, CollectionKind::File, TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert_eq!(repo.count(), 0);
}

#[tokio::test]
async fn given_name_of_exactly_255_bytes_when_create_then_accepted() {
    // The boundary itself is valid — 256 is the first rejected length.
    let (repo, h) = seeded();
    let at_limit = "n".repeat(255);

    let result = h
        .create(&at_limit, CollectionKind::File, TOKEN)
        .await
        .expect("create");

    assert_eq!(result.name, at_limit);
    assert_eq!(repo.count(), 1);
}

#[tokio::test]
async fn given_name_containing_nul_when_create_then_invalid_input() {
    // A stray NUL would terminate the C string before the FFI boundary saw the
    // whole name, so the two surfaces would store different things.
    let (repo, h) = seeded();

    let result = h.create("Favo\0rites", CollectionKind::File, TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert_eq!(repo.count(), 0);
}

// ---------------- AF-02: unauthorized ----------------

#[tokio::test]
async fn given_unauthenticated_when_create_then_unauthorized_and_nothing_persisted() {
    let repo = FakeCollectionRepository::new();
    let h = handler(FakeAuth::Denying, repo.clone());

    let result = h.create("Sci-fi novels", CollectionKind::File, "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
    assert_eq!(repo.count(), 0);
}

#[tokio::test]
async fn given_unauthenticated_and_invalid_name_when_create_then_unauthorized_not_invalid_input() {
    // Authentication is evaluated before the payload (FR-AU-07 / SRD §7): an
    // unauthenticated caller must not learn that its name would have been
    // rejected.
    let repo = FakeCollectionRepository::new();
    let h = handler(FakeAuth::Denying, repo.clone());

    let result = h.create("", CollectionKind::File, "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
    assert_eq!(repo.count(), 0);
}

// ---------------- Repository write failure ----------------

#[tokio::test]
async fn given_create_when_repo_write_fails_then_error_propagated_and_nothing_persisted() {
    let repo = FakeCollectionRepository::new();
    repo.fail_inserts();
    let h = handler(FakeAuth::Allowing, repo.clone());

    let result = h.create("Sci-fi novels", CollectionKind::File, TOKEN).await;

    assert!(matches!(result, Err(DomainError::Internal(_))));
    assert_eq!(repo.count(), 0);
}
