//! Unit tests for the UC-15 CreateBookmarkHandler (Testing Specification
//! §6). Each test exercises exactly the handler against trait fakes — no real
//! DB or auth service. Coverage follows §6.3: happy path (with and without a
//! collection), every url/title validation failure (AF-01), the
//! wrong-kind/missing collection branch (AF-02), the unauthorized branch
//! (AF-03), and the repository-write-failure branch.

use uuid::Uuid;

use alexandria_core::bookmarks::commands::create::CreateBookmarkHandler;
use alexandria_core::collections::model::{Collection, CollectionKind};
use alexandria_core::errors::DomainError;

use crate::common::{FakeAuth, FakeBookmarkRepository, FakeCollectionRepository};

const TOKEN: &str = "bearer-token";

fn handler(
    auth: FakeAuth,
    bookmark_repo: FakeBookmarkRepository,
    collection_repo: FakeCollectionRepository,
) -> CreateBookmarkHandler<FakeAuth, FakeBookmarkRepository, FakeCollectionRepository> {
    CreateBookmarkHandler::new(auth, bookmark_repo, collection_repo)
}

fn seeded() -> (
    FakeBookmarkRepository,
    CreateBookmarkHandler<FakeAuth, FakeBookmarkRepository, FakeCollectionRepository>,
) {
    let bookmark_repo = FakeBookmarkRepository::new();
    let h = handler(
        FakeAuth::Allowing,
        bookmark_repo.clone(),
        FakeCollectionRepository::new(),
    );
    (bookmark_repo, h)
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_valid_url_and_title_when_create_then_bookmark_persisted_and_returned() {
    let (repo, h) = seeded();

    let result = h
        .create("https://example.com/article", "An article", None, TOKEN)
        .await
        .expect("create");

    assert_eq!(result.url, "https://example.com/article");
    assert_eq!(result.title, "An article");
    assert_eq!(result.collection_uuid, None);
    assert!(!result.uuid.is_nil(), "a uuid was minted");

    let persisted = repo.bookmark_for(result.uuid).expect("persisted");
    assert_eq!(persisted, result, "the returned record is the stored one");
    assert_eq!(repo.count(), 1);
}

#[tokio::test]
async fn given_bookmark_collection_when_create_then_linked_to_collection() {
    let bookmark_repo = FakeBookmarkRepository::new();
    let collection_repo = FakeCollectionRepository::new();
    let collection_uuid = Uuid::new_v4();
    collection_repo.seed(Collection {
        uuid: collection_uuid,
        name: "Reading list".to_string(),
        kind: CollectionKind::Bookmark,
    });
    let h = handler(FakeAuth::Allowing, bookmark_repo.clone(), collection_repo);

    let result = h
        .create(
            "https://example.com",
            "Example",
            Some(collection_uuid),
            TOKEN,
        )
        .await
        .expect("create");

    assert_eq!(result.collection_uuid, Some(collection_uuid));
    assert_eq!(
        bookmark_repo
            .bookmark_for(result.uuid)
            .unwrap()
            .collection_uuid,
        Some(collection_uuid)
    );
}

// ---------------- AF-01: invalid input (url / title) ----------------

#[tokio::test]
async fn given_empty_url_when_create_then_invalid_input_and_nothing_persisted() {
    let (repo, h) = seeded();

    let result = h.create("", "Title", None, TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert_eq!(repo.count(), 0);
}

#[tokio::test]
async fn given_url_without_scheme_when_create_then_invalid_input() {
    let (repo, h) = seeded();

    let result = h
        .create("example.com/no-scheme", "Title", None, TOKEN)
        .await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert_eq!(repo.count(), 0);
}

#[tokio::test]
async fn given_url_with_empty_scheme_when_create_then_invalid_input() {
    let (repo, h) = seeded();

    let result = h.create("://example.com", "Title", None, TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert_eq!(repo.count(), 0);
}

#[tokio::test]
async fn given_url_with_nothing_after_scheme_when_create_then_invalid_input() {
    let (repo, h) = seeded();

    let result = h.create("https://", "Title", None, TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert_eq!(repo.count(), 0);
}

#[tokio::test]
async fn given_untrimmed_url_when_create_then_invalid_input_rather_than_silent_trim() {
    let (repo, h) = seeded();

    let result = h
        .create(" https://example.com ", "Title", None, TOKEN)
        .await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert_eq!(repo.count(), 0);
}

#[tokio::test]
async fn given_empty_title_when_create_then_invalid_input_and_nothing_persisted() {
    let (repo, h) = seeded();

    let result = h.create("https://example.com", "", None, TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert_eq!(repo.count(), 0);
}

#[tokio::test]
async fn given_whitespace_only_title_when_create_then_invalid_input() {
    let (repo, h) = seeded();

    let result = h.create("https://example.com", "   ", None, TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert_eq!(repo.count(), 0);
}

// ---------------- AF-02: referenced collection ----------------

#[tokio::test]
async fn given_collection_of_wrong_kind_when_create_then_invalid_input_and_nothing_persisted() {
    let bookmark_repo = FakeBookmarkRepository::new();
    let collection_repo = FakeCollectionRepository::new();
    let collection_uuid = Uuid::new_v4();
    collection_repo.seed(Collection {
        uuid: collection_uuid,
        name: "My files".to_string(),
        kind: CollectionKind::File,
    });
    let h = handler(FakeAuth::Allowing, bookmark_repo.clone(), collection_repo);

    let result = h
        .create(
            "https://example.com",
            "Example",
            Some(collection_uuid),
            TOKEN,
        )
        .await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert_eq!(bookmark_repo.count(), 0);
}

#[tokio::test]
async fn given_unknown_collection_uuid_when_create_then_not_found_and_nothing_persisted() {
    let (repo, h) = seeded();

    let result = h
        .create(
            "https://example.com",
            "Example",
            Some(Uuid::new_v4()),
            TOKEN,
        )
        .await;

    assert!(matches!(result, Err(DomainError::NotFound)));
    assert_eq!(repo.count(), 0);
}

// ---------------- AF-03: unauthorized ----------------

#[tokio::test]
async fn given_unauthenticated_when_create_then_unauthorized_and_nothing_persisted() {
    let bookmark_repo = FakeBookmarkRepository::new();
    let h = handler(
        FakeAuth::Denying,
        bookmark_repo.clone(),
        FakeCollectionRepository::new(),
    );

    let result = h.create("https://example.com", "Example", None, "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
    assert_eq!(bookmark_repo.count(), 0);
}

#[tokio::test]
async fn given_unauthenticated_and_invalid_url_when_create_then_unauthorized_not_invalid_input() {
    // Authentication is evaluated before the payload (FR-AU-07 / SRD §7): an
    // unauthenticated caller must not learn that its url would have been
    // rejected.
    let bookmark_repo = FakeBookmarkRepository::new();
    let h = handler(
        FakeAuth::Denying,
        bookmark_repo.clone(),
        FakeCollectionRepository::new(),
    );

    let result = h.create("", "", None, "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
    assert_eq!(bookmark_repo.count(), 0);
}

// ---------------- Repository write failure ----------------

#[tokio::test]
async fn given_create_when_repo_write_fails_then_error_propagated_and_nothing_persisted() {
    let bookmark_repo = FakeBookmarkRepository::new();
    bookmark_repo.fail_inserts();
    let h = handler(
        FakeAuth::Allowing,
        bookmark_repo.clone(),
        FakeCollectionRepository::new(),
    );

    let result = h
        .create("https://example.com", "Example", None, TOKEN)
        .await;

    assert!(matches!(result, Err(DomainError::Internal(_))));
    assert_eq!(bookmark_repo.count(), 0);
}
