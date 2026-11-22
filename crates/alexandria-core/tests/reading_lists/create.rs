//! Unit tests for the UC-26 CreateReadingListHandler (Testing Specification
//! §6). Each test exercises exactly the handler against trait fakes — no real
//! DB. Coverage follows §6.3: happy path, every name-validation failure
//! (AF-01), and the repository-write-failure branch.

use alexandria_core::errors::DomainError;
use alexandria_core::reading_lists::commands::create::CreateReadingListHandler;

use crate::common::FakeReadingListRepository;

fn handler(repo: FakeReadingListRepository) -> CreateReadingListHandler<FakeReadingListRepository> {
    CreateReadingListHandler::new(repo)
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_valid_name_when_create_then_reading_list_persisted_and_returned() {
    let repo = FakeReadingListRepository::new();
    let h = handler(repo.clone());

    let result = h.create("Summer reads").await.expect("create");

    assert_eq!(result.name, "Summer reads");
    assert!(!result.uuid.is_nil(), "a uuid was minted");
    let persisted = repo.reading_list_for(result.uuid).expect("persisted");
    assert_eq!(persisted, result);
    assert_eq!(repo.count(), 1);
}

#[tokio::test]
async fn given_two_reading_lists_with_same_name_when_create_then_both_get_distinct_uuids() {
    let repo = FakeReadingListRepository::new();
    let h = handler(repo.clone());

    let first = h.create("Favorites").await.expect("first");
    let second = h.create("Favorites").await.expect("second");

    assert_ne!(first.uuid, second.uuid);
    assert_eq!(repo.count(), 2);
}

// ---------------- AF-01: invalid input (name) ----------------

#[tokio::test]
async fn given_empty_name_when_create_then_invalid_input_and_nothing_persisted() {
    let repo = FakeReadingListRepository::new();
    let h = handler(repo.clone());

    let result = h.create("").await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert_eq!(repo.count(), 0);
}

#[tokio::test]
async fn given_whitespace_only_name_when_create_then_invalid_input() {
    let repo = FakeReadingListRepository::new();
    let h = handler(repo.clone());

    let result = h.create("   ").await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert_eq!(repo.count(), 0);
}

#[tokio::test]
async fn given_untrimmed_name_when_create_then_invalid_input_rather_than_silent_trim() {
    let repo = FakeReadingListRepository::new();
    let h = handler(repo.clone());

    let result = h.create("  Favorites  ").await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert_eq!(repo.count(), 0);
}

#[tokio::test]
async fn given_name_longer_than_255_bytes_when_create_then_invalid_input() {
    let repo = FakeReadingListRepository::new();
    let h = handler(repo.clone());
    let long = "n".repeat(256);

    let result = h.create(&long).await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert_eq!(repo.count(), 0);
}

// ---------------- Repository write failure ----------------

#[tokio::test]
async fn given_create_when_repo_write_fails_then_error_propagated_and_nothing_persisted() {
    let repo = FakeReadingListRepository::new();
    repo.fail_inserts();
    let h = handler(repo.clone());

    let result = h.create("Summer reads").await;

    assert!(matches!(result, Err(DomainError::Internal(_))));
    assert_eq!(repo.count(), 0);
}
