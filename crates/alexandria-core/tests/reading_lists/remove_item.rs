//! Unit tests for the UC-30 RemoveItemFromReadingListHandler (Testing
//! Specification §6). Each test exercises exactly the handler against
//! trait fakes — no real DB or auth service. Coverage follows §6.3: happy
//! path, not-found (AF-01), and the unauthorized branch (AF-02).

use alexandria_core::catalog::model::FileType;
use alexandria_core::errors::DomainError;
use alexandria_core::reading_lists::commands::add_item::AddItemToReadingListHandler;
use alexandria_core::reading_lists::commands::create::CreateReadingListHandler;
use alexandria_core::reading_lists::commands::remove_item::RemoveItemFromReadingListHandler;
use alexandria_core::reading_lists::repos::ReadingListRepository;

use crate::common::{existing_file, FakeAuth, FakeCatalogRepository, FakeReadingListRepository};

const TOKEN: &str = "bearer-token";

fn handler(
    auth: FakeAuth,
    repo: FakeReadingListRepository,
) -> RemoveItemFromReadingListHandler<FakeAuth, FakeReadingListRepository> {
    RemoveItemFromReadingListHandler::new(auth, repo)
}

async fn seeded_linked(reading_list_repo: &FakeReadingListRepository) -> (uuid::Uuid, uuid::Uuid) {
    let create_handler =
        CreateReadingListHandler::new(FakeAuth::Allowing, reading_list_repo.clone());
    let reading_list = create_handler
        .create("Summer reads", TOKEN)
        .await
        .expect("create reading list");

    let catalog_repo = FakeCatalogRepository::new();
    let doc = existing_file("/books/a.pdf", FileType::Document);
    let item_uuid = doc.uuid;
    catalog_repo.seed(doc);
    let add_handler = AddItemToReadingListHandler::new(
        FakeAuth::Allowing,
        reading_list_repo.clone(),
        catalog_repo,
    );
    add_handler
        .add(reading_list.uuid, item_uuid, TOKEN)
        .await
        .expect("link item");

    (reading_list.uuid, item_uuid)
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_linked_item_when_removed_then_confirmation_and_progress_gone() {
    let reading_list_repo = FakeReadingListRepository::new();
    let (reading_list_uuid, item_uuid) = seeded_linked(&reading_list_repo).await;
    let h = handler(FakeAuth::Allowing, reading_list_repo.clone());

    let result = h
        .remove(reading_list_uuid, item_uuid, TOKEN)
        .await
        .expect("remove");

    assert_eq!(result.reading_list_uuid, reading_list_uuid);
    assert_eq!(result.item_uuid, item_uuid);
    assert!(reading_list_repo
        .find_progress(reading_list_uuid, item_uuid)
        .await
        .expect("find")
        .is_none());
}

// ---------------- AF-01: not found ----------------

#[tokio::test]
async fn given_item_not_on_reading_list_when_removed_then_not_found() {
    let reading_list_repo = FakeReadingListRepository::new();
    let create_handler =
        CreateReadingListHandler::new(FakeAuth::Allowing, reading_list_repo.clone());
    let reading_list = create_handler
        .create("Summer reads", TOKEN)
        .await
        .expect("create reading list");
    let h = handler(FakeAuth::Allowing, reading_list_repo);

    let result = h
        .remove(reading_list.uuid, uuid::Uuid::new_v4(), TOKEN)
        .await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

#[tokio::test]
async fn given_already_removed_item_when_removed_again_then_not_found() {
    let reading_list_repo = FakeReadingListRepository::new();
    let (reading_list_uuid, item_uuid) = seeded_linked(&reading_list_repo).await;
    let h = handler(FakeAuth::Allowing, reading_list_repo);
    h.remove(reading_list_uuid, item_uuid, TOKEN)
        .await
        .expect("first remove");

    let result = h.remove(reading_list_uuid, item_uuid, TOKEN).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

// ---------------- AF-02: unauthorized ----------------

#[tokio::test]
async fn given_unauthenticated_when_removed_then_unauthorized() {
    let reading_list_repo = FakeReadingListRepository::new();
    let h = handler(FakeAuth::Denying, reading_list_repo);

    let result = h
        .remove(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), "")
        .await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

#[tokio::test]
async fn given_unauthenticated_and_unknown_progress_when_removed_then_unauthorized_not_not_found() {
    let reading_list_repo = FakeReadingListRepository::new();
    let h = handler(FakeAuth::Denying, reading_list_repo);

    let result = h
        .remove(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), "")
        .await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}
