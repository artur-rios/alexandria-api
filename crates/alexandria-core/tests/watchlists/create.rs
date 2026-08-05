//! Unit tests for the UC-20 CreateWatchlistHandler (Testing Specification
//! §6). Each test exercises exactly the handler against trait fakes — no
//! real DB or auth service. Coverage follows §6.3: happy path, every
//! name-validation failure (AF-01), the unauthorized branch (AF-02), and
//! the repository-write-failure branch.

use alexandria_core::errors::DomainError;
use alexandria_core::watchlists::commands::create::CreateWatchlistHandler;

use crate::common::{FakeAuth, FakeWatchlistRepository};

const TOKEN: &str = "bearer-token";

fn handler(
    auth: FakeAuth,
    repo: FakeWatchlistRepository,
) -> CreateWatchlistHandler<FakeAuth, FakeWatchlistRepository> {
    CreateWatchlistHandler::new(auth, repo)
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_valid_name_when_create_then_watchlist_persisted_and_returned() {
    let repo = FakeWatchlistRepository::new();
    let h = handler(FakeAuth::Allowing, repo.clone());

    let result = h.create("Weekend movies", TOKEN).await.expect("create");

    assert_eq!(result.name, "Weekend movies");
    assert!(!result.uuid.is_nil(), "a uuid was minted");
    let persisted = repo.watchlist_for(result.uuid).expect("persisted");
    assert_eq!(persisted, result);
    assert_eq!(repo.count(), 1);
}

#[tokio::test]
async fn given_two_watchlists_with_same_name_when_create_then_both_get_distinct_uuids() {
    let repo = FakeWatchlistRepository::new();
    let h = handler(FakeAuth::Allowing, repo.clone());

    let first = h.create("Favorites", TOKEN).await.expect("first");
    let second = h.create("Favorites", TOKEN).await.expect("second");

    assert_ne!(first.uuid, second.uuid);
    assert_eq!(repo.count(), 2);
}

// ---------------- AF-01: invalid input (name) ----------------

#[tokio::test]
async fn given_empty_name_when_create_then_invalid_input_and_nothing_persisted() {
    let repo = FakeWatchlistRepository::new();
    let h = handler(FakeAuth::Allowing, repo.clone());

    let result = h.create("", TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert_eq!(repo.count(), 0);
}

#[tokio::test]
async fn given_whitespace_only_name_when_create_then_invalid_input() {
    let repo = FakeWatchlistRepository::new();
    let h = handler(FakeAuth::Allowing, repo.clone());

    let result = h.create("   ", TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert_eq!(repo.count(), 0);
}

#[tokio::test]
async fn given_untrimmed_name_when_create_then_invalid_input_rather_than_silent_trim() {
    let repo = FakeWatchlistRepository::new();
    let h = handler(FakeAuth::Allowing, repo.clone());

    let result = h.create("  Favorites  ", TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert_eq!(repo.count(), 0);
}

#[tokio::test]
async fn given_name_longer_than_255_bytes_when_create_then_invalid_input() {
    let repo = FakeWatchlistRepository::new();
    let h = handler(FakeAuth::Allowing, repo.clone());
    let long = "n".repeat(256);

    let result = h.create(&long, TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert_eq!(repo.count(), 0);
}

// ---------------- AF-02: unauthorized ----------------

#[tokio::test]
async fn given_unauthenticated_when_create_then_unauthorized_and_nothing_persisted() {
    let repo = FakeWatchlistRepository::new();
    let h = handler(FakeAuth::Denying, repo.clone());

    let result = h.create("Weekend movies", "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
    assert_eq!(repo.count(), 0);
}

#[tokio::test]
async fn given_unauthenticated_and_invalid_name_when_create_then_unauthorized_not_invalid_input() {
    let repo = FakeWatchlistRepository::new();
    let h = handler(FakeAuth::Denying, repo.clone());

    let result = h.create("", "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
    assert_eq!(repo.count(), 0);
}

// ---------------- Repository write failure ----------------

#[tokio::test]
async fn given_create_when_repo_write_fails_then_error_propagated_and_nothing_persisted() {
    let repo = FakeWatchlistRepository::new();
    repo.fail_inserts();
    let h = handler(FakeAuth::Allowing, repo.clone());

    let result = h.create("Weekend movies", TOKEN).await;

    assert!(matches!(result, Err(DomainError::Internal(_))));
    assert_eq!(repo.count(), 0);
}
