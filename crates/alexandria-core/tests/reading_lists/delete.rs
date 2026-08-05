//! Unit tests for the UC-31 DeleteReadingListHandler (Testing
//! Specification §6). Each test exercises exactly the handler against
//! trait fakes — no real DB or auth service. Coverage follows §6.3: happy
//! path (with and without linked items), not-found (AF-01), and the
//! unauthorized branch (AF-02).

use alexandria_core::catalog::model::FileType;
use alexandria_core::catalog::repos::CatalogRepository;
use alexandria_core::errors::DomainError;
use alexandria_core::reading_lists::commands::add_item::AddItemToReadingListHandler;
use alexandria_core::reading_lists::commands::create::CreateReadingListHandler;
use alexandria_core::reading_lists::commands::delete::DeleteReadingListHandler;
use alexandria_core::reading_lists::repos::ReadingListRepository;

use crate::common::{existing_file, FakeAuth, FakeCatalogRepository, FakeReadingListRepository};

const TOKEN: &str = "bearer-token";

fn handler(
    auth: FakeAuth,
    repo: FakeReadingListRepository,
) -> DeleteReadingListHandler<FakeAuth, FakeReadingListRepository> {
    DeleteReadingListHandler::new(auth, repo)
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_empty_reading_list_when_deleted_then_predelete_record_returned_and_row_removed() {
    let reading_list_repo = FakeReadingListRepository::new();
    let create_handler =
        CreateReadingListHandler::new(FakeAuth::Allowing, reading_list_repo.clone());
    let reading_list = create_handler
        .create("Summer reads", TOKEN)
        .await
        .expect("create reading list");
    let h = handler(FakeAuth::Allowing, reading_list_repo.clone());

    let result = h.delete(reading_list.uuid, TOKEN).await.expect("delete");

    assert_eq!(result, reading_list);
    assert!(reading_list_repo
        .find_by_uuid(reading_list.uuid)
        .await
        .expect("find")
        .is_none());
}

#[tokio::test]
async fn given_reading_list_with_linked_item_when_deleted_then_progress_gone_and_item_preserved() {
    let reading_list_repo = FakeReadingListRepository::new();
    let create_handler =
        CreateReadingListHandler::new(FakeAuth::Allowing, reading_list_repo.clone());
    let reading_list = create_handler
        .create("Summer reads", TOKEN)
        .await
        .expect("create reading list");

    let catalog_repo = FakeCatalogRepository::new();
    let doc = existing_file("/books/a.pdf", FileType::Document);
    let item_uuid = doc.uuid;
    catalog_repo.seed(doc.clone());
    let add_handler = AddItemToReadingListHandler::new(
        FakeAuth::Allowing,
        reading_list_repo.clone(),
        catalog_repo.clone(),
    );
    add_handler
        .add(reading_list.uuid, item_uuid, TOKEN)
        .await
        .expect("link item");

    let h = handler(FakeAuth::Allowing, reading_list_repo.clone());

    h.delete(reading_list.uuid, TOKEN).await.expect("delete");

    assert!(reading_list_repo
        .find_progress(reading_list.uuid, item_uuid)
        .await
        .expect("find progress")
        .is_none());
    assert!(
        catalog_repo
            .find_by_uuid(item_uuid)
            .await
            .expect("find item")
            .is_some(),
        "the file itself is preserved"
    );
}

// ---------------- AF-01: not found ----------------

#[tokio::test]
async fn given_unknown_uuid_when_deleted_then_not_found() {
    let reading_list_repo = FakeReadingListRepository::new();
    let h = handler(FakeAuth::Allowing, reading_list_repo);

    let result = h.delete(uuid::Uuid::new_v4(), TOKEN).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

// ---------------- AF-02: unauthorized ----------------

#[tokio::test]
async fn given_unauthenticated_when_deleted_then_unauthorized() {
    let reading_list_repo = FakeReadingListRepository::new();
    let h = handler(FakeAuth::Denying, reading_list_repo);

    let result = h.delete(uuid::Uuid::new_v4(), "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

#[tokio::test]
async fn given_unauthenticated_and_unknown_uuid_when_deleted_then_unauthorized_not_not_found() {
    let reading_list_repo = FakeReadingListRepository::new();
    let h = handler(FakeAuth::Denying, reading_list_repo);

    let result = h.delete(uuid::Uuid::new_v4(), "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}
