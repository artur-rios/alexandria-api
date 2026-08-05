//! Unit tests for the UC-29 UpdateReadingProgressHandler (Testing
//! Specification §6). Each test exercises exactly the handler against
//! trait fakes — no real DB or auth service. Coverage follows §6.3: happy
//! path (both valid transitions, issue tracking), every invalid transition
//! (AF-01), not-found (AF-02, item not on the reading list), and the
//! unauthorized branch (AF-03).

use alexandria_core::catalog::model::FileType;
use alexandria_core::errors::DomainError;
use alexandria_core::reading_lists::commands::add_item::AddItemToReadingListHandler;
use alexandria_core::reading_lists::commands::create::CreateReadingListHandler;
use alexandria_core::reading_lists::commands::update_progress::{
    is_valid_transition, UpdateReadingProgressHandler,
};
use alexandria_core::reading_lists::model::ReadingState;

use crate::common::{existing_file, FakeAuth, FakeCatalogRepository, FakeReadingListRepository};

const TOKEN: &str = "bearer-token";

fn handler(
    auth: FakeAuth,
    repo: FakeReadingListRepository,
) -> UpdateReadingProgressHandler<FakeAuth, FakeReadingListRepository> {
    UpdateReadingProgressHandler::new(auth, repo)
}

/// A reading list with one linked (Pending) comic, ready for a transition.
async fn seeded_pending(
    reading_list_repo: &FakeReadingListRepository,
    catalog_repo: FakeCatalogRepository,
) -> (uuid::Uuid, uuid::Uuid) {
    let create_handler =
        CreateReadingListHandler::new(FakeAuth::Allowing, reading_list_repo.clone());
    let reading_list = create_handler
        .create("Summer reads", TOKEN)
        .await
        .expect("create reading list");

    let comic = existing_file("/comics/a.cbz", FileType::Comic);
    let item_uuid = comic.uuid;
    catalog_repo.seed(comic);
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

// ---------------- is_valid_transition ----------------

#[test]
fn given_forward_transitions_when_checked_then_valid() {
    assert!(is_valid_transition(
        ReadingState::Pending,
        ReadingState::Reading
    ));
    assert!(is_valid_transition(
        ReadingState::Reading,
        ReadingState::Read
    ));
}

#[test]
fn given_backward_same_or_skipped_transitions_when_checked_then_invalid() {
    assert!(!is_valid_transition(
        ReadingState::Read,
        ReadingState::Pending
    ));
    assert!(!is_valid_transition(
        ReadingState::Reading,
        ReadingState::Pending
    ));
    assert!(!is_valid_transition(
        ReadingState::Read,
        ReadingState::Reading
    ));
    assert!(!is_valid_transition(
        ReadingState::Pending,
        ReadingState::Read
    ));
    assert!(!is_valid_transition(
        ReadingState::Pending,
        ReadingState::Pending
    ));
    assert!(!is_valid_transition(
        ReadingState::Reading,
        ReadingState::Reading
    ));
    assert!(!is_valid_transition(ReadingState::Read, ReadingState::Read));
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_pending_when_updated_to_reading_then_state_updated() {
    let reading_list_repo = FakeReadingListRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let (reading_list_uuid, item_uuid) = seeded_pending(&reading_list_repo, catalog_repo).await;
    let h = handler(FakeAuth::Allowing, reading_list_repo);

    let result = h
        .update(
            reading_list_uuid,
            item_uuid,
            ReadingState::Reading,
            None,
            None,
            TOKEN,
        )
        .await
        .expect("update");

    assert_eq!(result.state, ReadingState::Reading);
}

#[tokio::test]
async fn given_reading_when_updated_to_read_then_state_updated() {
    let reading_list_repo = FakeReadingListRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let (reading_list_uuid, item_uuid) = seeded_pending(&reading_list_repo, catalog_repo).await;
    let h = handler(FakeAuth::Allowing, reading_list_repo);
    h.update(
        reading_list_uuid,
        item_uuid,
        ReadingState::Reading,
        None,
        None,
        TOKEN,
    )
    .await
    .expect("first update");

    let result = h
        .update(
            reading_list_uuid,
            item_uuid,
            ReadingState::Read,
            None,
            None,
            TOKEN,
        )
        .await
        .expect("second update");

    assert_eq!(result.state, ReadingState::Read);
}

#[tokio::test]
async fn given_comic_issue_when_updated_then_issue_recorded() {
    let reading_list_repo = FakeReadingListRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let (reading_list_uuid, item_uuid) = seeded_pending(&reading_list_repo, catalog_repo).await;
    let h = handler(FakeAuth::Allowing, reading_list_repo);

    let result = h
        .update(
            reading_list_uuid,
            item_uuid,
            ReadingState::Reading,
            Some(3),
            Some(12),
            TOKEN,
        )
        .await
        .expect("update");

    assert_eq!(result.current_issue, Some(3));
    assert_eq!(result.total_issues, Some(12));
}

#[tokio::test]
async fn given_issue_fields_omitted_when_updated_then_cleared_not_left_untouched() {
    let reading_list_repo = FakeReadingListRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let (reading_list_uuid, item_uuid) = seeded_pending(&reading_list_repo, catalog_repo).await;
    let h = handler(FakeAuth::Allowing, reading_list_repo);
    h.update(
        reading_list_uuid,
        item_uuid,
        ReadingState::Reading,
        Some(3),
        Some(12),
        TOKEN,
    )
    .await
    .expect("first update");

    let result = h
        .update(
            reading_list_uuid,
            item_uuid,
            ReadingState::Read,
            None,
            None,
            TOKEN,
        )
        .await
        .expect("second update");

    assert_eq!(result.current_issue, None);
    assert_eq!(result.total_issues, None);
}

// ---------------- AF-01: invalid transition ----------------

#[tokio::test]
async fn given_backward_transition_when_updated_then_invalid_state() {
    let reading_list_repo = FakeReadingListRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let (reading_list_uuid, item_uuid) = seeded_pending(&reading_list_repo, catalog_repo).await;
    let h = handler(FakeAuth::Allowing, reading_list_repo);

    let result = h
        .update(
            reading_list_uuid,
            item_uuid,
            ReadingState::Read,
            None,
            None,
            TOKEN,
        )
        .await;

    assert!(matches!(result, Err(DomainError::InvalidState)));
}

#[tokio::test]
async fn given_resubmitted_same_state_when_updated_then_invalid_state() {
    let reading_list_repo = FakeReadingListRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let (reading_list_uuid, item_uuid) = seeded_pending(&reading_list_repo, catalog_repo).await;
    let h = handler(FakeAuth::Allowing, reading_list_repo);

    let result = h
        .update(
            reading_list_uuid,
            item_uuid,
            ReadingState::Pending,
            None,
            None,
            TOKEN,
        )
        .await;

    assert!(matches!(result, Err(DomainError::InvalidState)));
}

// ---------------- AF-02: not found ----------------

#[tokio::test]
async fn given_item_not_on_reading_list_when_updated_then_not_found() {
    let reading_list_repo = FakeReadingListRepository::new();
    let create_handler =
        CreateReadingListHandler::new(FakeAuth::Allowing, reading_list_repo.clone());
    let reading_list = create_handler
        .create("Summer reads", TOKEN)
        .await
        .expect("create reading list");
    let h = handler(FakeAuth::Allowing, reading_list_repo);

    let result = h
        .update(
            reading_list.uuid,
            uuid::Uuid::new_v4(),
            ReadingState::Reading,
            None,
            None,
            TOKEN,
        )
        .await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

// ---------------- AF-03: unauthorized ----------------

#[tokio::test]
async fn given_unauthenticated_when_updated_then_unauthorized() {
    let reading_list_repo = FakeReadingListRepository::new();
    let h = handler(FakeAuth::Denying, reading_list_repo);

    let result = h
        .update(
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            ReadingState::Reading,
            None,
            None,
            "",
        )
        .await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

#[tokio::test]
async fn given_unauthenticated_and_unknown_progress_when_updated_then_unauthorized_not_not_found() {
    let reading_list_repo = FakeReadingListRepository::new();
    let h = handler(FakeAuth::Denying, reading_list_repo);

    let result = h
        .update(
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            ReadingState::Reading,
            None,
            None,
            "",
        )
        .await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}
