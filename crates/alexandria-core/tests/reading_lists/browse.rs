//! Unit tests for the UC-27 BrowseReadingListsHandler (Testing
//! Specification §6). Each test exercises exactly the handler against
//! trait fakes — no real DB or auth service. Coverage follows §6.3: happy
//! path (all reading lists, single reading list), not-found (AF-01), and
//! the unauthorized branch (AF-02).

use alexandria_core::catalog::model::FileType;
use alexandria_core::errors::DomainError;
use alexandria_core::reading_lists::commands::add_item::AddItemToReadingListHandler;
use alexandria_core::reading_lists::model::{NewReadingList, ReadingState};
use alexandria_core::reading_lists::queries::browse::BrowseReadingListsHandler;
use alexandria_core::reading_lists::repos::ReadingListRepository;

use crate::common::{existing_file, FakeAuth, FakeCatalogRepository, FakeReadingListRepository};

const TOKEN: &str = "bearer-token";

fn handler(
    auth: FakeAuth,
    repo: FakeReadingListRepository,
) -> BrowseReadingListsHandler<FakeAuth, FakeReadingListRepository> {
    BrowseReadingListsHandler::new(auth, repo)
}

async fn seeded_reading_list(repo: &FakeReadingListRepository, name: &str) -> uuid::Uuid {
    let reading_list = repo
        .insert_reading_list(NewReadingList {
            uuid: uuid::Uuid::new_v4(),
            name: name.to_string(),
        })
        .await
        .expect("seed reading list");
    reading_list.uuid
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_no_filter_when_listed_then_every_reading_list_returned_with_progress() {
    let reading_list_repo = FakeReadingListRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let a_uuid = seeded_reading_list(&reading_list_repo, "A list").await;
    let b_uuid = seeded_reading_list(&reading_list_repo, "B list").await;

    let doc = existing_file("/books/a.pdf", FileType::Document);
    let item_uuid = doc.uuid;
    catalog_repo.seed(doc);
    let add_handler = AddItemToReadingListHandler::new(
        FakeAuth::Allowing,
        reading_list_repo.clone(),
        catalog_repo,
    );
    add_handler
        .add(a_uuid, item_uuid, TOKEN)
        .await
        .expect("link item");

    let h = handler(FakeAuth::Allowing, reading_list_repo);

    let result = h.list(None, TOKEN).await.expect("list");

    assert_eq!(result.len(), 2);
    assert_eq!(result[0].uuid, a_uuid);
    assert_eq!(result[0].items.len(), 1);
    assert_eq!(result[0].items[0].item_uuid, item_uuid);
    assert_eq!(result[0].items[0].state, ReadingState::Pending);
    assert_eq!(result[1].uuid, b_uuid);
    assert!(result[1].items.is_empty());
}

#[tokio::test]
async fn given_reading_list_uuid_when_listed_then_only_that_reading_list_returned() {
    let reading_list_repo = FakeReadingListRepository::new();
    let a_uuid = seeded_reading_list(&reading_list_repo, "A list").await;
    let _b_uuid = seeded_reading_list(&reading_list_repo, "B list").await;
    let h = handler(FakeAuth::Allowing, reading_list_repo);

    let result = h.list(Some(a_uuid), TOKEN).await.expect("list");

    assert_eq!(result.len(), 1);
    assert_eq!(result[0].uuid, a_uuid);
}

#[tokio::test]
async fn given_no_reading_lists_when_listed_then_empty_array() {
    let reading_list_repo = FakeReadingListRepository::new();
    let h = handler(FakeAuth::Allowing, reading_list_repo);

    let result = h.list(None, TOKEN).await.expect("list");

    assert!(result.is_empty());
}

// ---------------- AF-01: not found ----------------

#[tokio::test]
async fn given_unknown_reading_list_uuid_when_listed_then_not_found() {
    let reading_list_repo = FakeReadingListRepository::new();
    let h = handler(FakeAuth::Allowing, reading_list_repo);

    let result = h.list(Some(uuid::Uuid::new_v4()), TOKEN).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

// ---------------- AF-02: unauthorized ----------------

#[tokio::test]
async fn given_unauthenticated_when_listed_then_unauthorized() {
    let reading_list_repo = FakeReadingListRepository::new();
    let h = handler(FakeAuth::Denying, reading_list_repo);

    let result = h.list(None, "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

#[tokio::test]
async fn given_unauthenticated_and_unknown_uuid_when_listed_then_unauthorized_not_not_found() {
    let reading_list_repo = FakeReadingListRepository::new();
    let h = handler(FakeAuth::Denying, reading_list_repo);

    let result = h.list(Some(uuid::Uuid::new_v4()), "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}
