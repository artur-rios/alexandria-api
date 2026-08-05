//! Unit tests for the UC-28 AddItemToReadingListHandler (Testing
//! Specification §6). Each test exercises exactly the handler against
//! trait fakes — no real DB or auth service. Coverage follows §6.3: happy
//! path (both eligible types, idempotent re-add), the ineligible-type
//! failure (AF-01), not-found (AF-02, reading list and item), and the
//! unauthorized branch (AF-03).

use alexandria_core::catalog::model::FileType;
use alexandria_core::errors::DomainError;
use alexandria_core::reading_lists::commands::add_item::AddItemToReadingListHandler;
use alexandria_core::reading_lists::model::{ReadingState, ReadingTargetKind};

use crate::common::{existing_file, FakeAuth, FakeCatalogRepository, FakeReadingListRepository};

const TOKEN: &str = "bearer-token";

fn handler(
    auth: FakeAuth,
    reading_list_repo: FakeReadingListRepository,
    catalog_repo: FakeCatalogRepository,
) -> AddItemToReadingListHandler<FakeAuth, FakeReadingListRepository, FakeCatalogRepository> {
    AddItemToReadingListHandler::new(auth, reading_list_repo, catalog_repo)
}

async fn seeded_reading_list(repo: &FakeReadingListRepository) -> uuid::Uuid {
    use alexandria_core::reading_lists::model::NewReadingList;
    use alexandria_core::reading_lists::repos::ReadingListRepository;

    let reading_list = repo
        .insert_reading_list(NewReadingList {
            uuid: uuid::Uuid::new_v4(),
            name: "Summer reads".to_string(),
        })
        .await
        .expect("seed reading list");
    reading_list.uuid
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_document_when_add_then_pending_progress_returned() {
    let reading_list_repo = FakeReadingListRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let reading_list_uuid = seeded_reading_list(&reading_list_repo).await;
    let doc = existing_file("/books/a.pdf", FileType::Document);
    let item_uuid = doc.uuid;
    catalog_repo.seed(doc);
    let h = handler(FakeAuth::Allowing, reading_list_repo, catalog_repo);

    let result = h
        .add(reading_list_uuid, item_uuid, TOKEN)
        .await
        .expect("add");

    assert_eq!(result.reading_list_uuid, reading_list_uuid);
    assert_eq!(result.item_uuid, item_uuid);
    assert_eq!(result.target_kind, ReadingTargetKind::Document);
    assert_eq!(result.state, ReadingState::Pending);
    assert_eq!(result.current_issue, None);
    assert_eq!(result.total_issues, None);
}

#[tokio::test]
async fn given_comic_when_add_then_comic_target_kind_recorded() {
    let reading_list_repo = FakeReadingListRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let reading_list_uuid = seeded_reading_list(&reading_list_repo).await;
    let comic = existing_file("/comics/a.cbz", FileType::Comic);
    let item_uuid = comic.uuid;
    catalog_repo.seed(comic);
    let h = handler(FakeAuth::Allowing, reading_list_repo, catalog_repo);

    let result = h
        .add(reading_list_uuid, item_uuid, TOKEN)
        .await
        .expect("add");

    assert_eq!(result.target_kind, ReadingTargetKind::Comic);
}

#[tokio::test]
async fn given_already_linked_item_when_add_again_then_idempotent() {
    let reading_list_repo = FakeReadingListRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let reading_list_uuid = seeded_reading_list(&reading_list_repo).await;
    let doc = existing_file("/books/a.pdf", FileType::Document);
    let item_uuid = doc.uuid;
    catalog_repo.seed(doc);
    let h = handler(FakeAuth::Allowing, reading_list_repo, catalog_repo);

    let first = h
        .add(reading_list_uuid, item_uuid, TOKEN)
        .await
        .expect("first add");
    let second = h
        .add(reading_list_uuid, item_uuid, TOKEN)
        .await
        .expect("second add");

    assert_eq!(
        first, second,
        "re-adding returns the same progress, not reset"
    );
}

// ---------------- AF-01: invalid input (ineligible type) ----------------

#[tokio::test]
async fn given_ineligible_file_when_add_then_invalid_input() {
    let reading_list_repo = FakeReadingListRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let reading_list_uuid = seeded_reading_list(&reading_list_repo).await;
    let video = existing_file("/videos/a.mp4", FileType::Video);
    let video_uuid = video.uuid;
    catalog_repo.seed(video);
    let h = handler(FakeAuth::Allowing, reading_list_repo, catalog_repo);

    let result = h.add(reading_list_uuid, video_uuid, TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
}

// ---------------- AF-02: not found ----------------

#[tokio::test]
async fn given_unknown_reading_list_when_add_then_not_found() {
    let reading_list_repo = FakeReadingListRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let doc = existing_file("/books/a.pdf", FileType::Document);
    let item_uuid = doc.uuid;
    catalog_repo.seed(doc);
    let h = handler(FakeAuth::Allowing, reading_list_repo, catalog_repo);

    let result = h.add(uuid::Uuid::new_v4(), item_uuid, TOKEN).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

#[tokio::test]
async fn given_unknown_item_when_add_then_not_found() {
    let reading_list_repo = FakeReadingListRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let reading_list_uuid = seeded_reading_list(&reading_list_repo).await;
    let h = handler(FakeAuth::Allowing, reading_list_repo, catalog_repo);

    let result = h.add(reading_list_uuid, uuid::Uuid::new_v4(), TOKEN).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

// ---------------- AF-03: unauthorized ----------------

#[tokio::test]
async fn given_unauthenticated_when_add_then_unauthorized() {
    let reading_list_repo = FakeReadingListRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let h = handler(FakeAuth::Denying, reading_list_repo, catalog_repo);

    let result = h.add(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

#[tokio::test]
async fn given_unauthenticated_and_unknown_reading_list_when_add_then_unauthorized_not_not_found() {
    let reading_list_repo = FakeReadingListRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let h = handler(FakeAuth::Denying, reading_list_repo, catalog_repo);

    let result = h.add(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}
