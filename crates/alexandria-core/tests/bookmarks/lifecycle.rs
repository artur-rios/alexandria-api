//! Unit tests for the UC-18 BookmarkLifecycleHandler (Testing Specification
//! §6). Each test exercises exactly the handler against trait fakes — no
//! real DB or auth service. Coverage follows §6.3: happy path for both
//! flows, AF-01 (not found) for both, the state-transition guards, and AF-02
//! (unauthorized) for both.

use uuid::Uuid;

use alexandria_core::bookmarks::commands::lifecycle::BookmarkLifecycleHandler;
use alexandria_core::bookmarks::model::{Bookmark, BookmarkState};
use alexandria_core::catalog::clock::FixedClock;
use alexandria_core::errors::DomainError;

use crate::common::{FakeAuth, FakeBookmarkRepository};

const TOKEN: &str = "bearer-token";

fn a_bookmark(uuid: Uuid, state: BookmarkState) -> Bookmark {
    Bookmark {
        uuid,
        url: "https://example.com".to_string(),
        title: "Example".to_string(),
        state,
        deleted_at: None,
        collection_uuid: None,
    }
}

fn clock() -> FixedClock {
    FixedClock(chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0).unwrap())
}

fn handler(
    auth: FakeAuth,
    repo: FakeBookmarkRepository,
) -> BookmarkLifecycleHandler<FakeAuth, FakeBookmarkRepository, FixedClock> {
    BookmarkLifecycleHandler::new(auth, repo, clock())
}

// ---------------- Main flow: soft_delete ----------------

#[tokio::test]
async fn given_active_bookmark_when_soft_delete_then_state_deleted_and_deleted_at_set() {
    let repo = FakeBookmarkRepository::new();
    let uuid = Uuid::new_v4();
    repo.seed(a_bookmark(uuid, BookmarkState::Active));
    let h = handler(FakeAuth::Allowing, repo.clone());

    let result = h.soft_delete(uuid, TOKEN).await.expect("soft delete");

    assert_eq!(result.state, BookmarkState::Deleted);
    assert_eq!(result.deleted_at, Some(clock().0));
    assert_eq!(
        repo.bookmark_for(uuid).unwrap().state,
        BookmarkState::Deleted
    );
}

#[tokio::test]
async fn given_deleted_bookmark_when_soft_delete_then_invalid_state() {
    let repo = FakeBookmarkRepository::new();
    let uuid = Uuid::new_v4();
    repo.seed(a_bookmark(uuid, BookmarkState::Deleted));
    let h = handler(FakeAuth::Allowing, repo);

    let result = h.soft_delete(uuid, TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidState)));
}

#[tokio::test]
async fn given_unknown_uuid_when_soft_delete_then_not_found() {
    let h = handler(FakeAuth::Allowing, FakeBookmarkRepository::new());

    let result = h.soft_delete(Uuid::new_v4(), TOKEN).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

#[tokio::test]
async fn given_unauthenticated_when_soft_delete_then_unauthorized() {
    let repo = FakeBookmarkRepository::new();
    let uuid = Uuid::new_v4();
    repo.seed(a_bookmark(uuid, BookmarkState::Active));
    let h = handler(FakeAuth::Denying, repo.clone());

    let result = h.soft_delete(uuid, "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
    assert_eq!(
        repo.bookmark_for(uuid).unwrap().state,
        BookmarkState::Active
    );
}

// ---------------- Main flow: restore ----------------

#[tokio::test]
async fn given_deleted_bookmark_when_restore_then_state_active_and_deleted_at_cleared() {
    let repo = FakeBookmarkRepository::new();
    let uuid = Uuid::new_v4();
    let mut bookmark = a_bookmark(uuid, BookmarkState::Deleted);
    bookmark.deleted_at = Some(clock().0);
    repo.seed(bookmark);
    let h = handler(FakeAuth::Allowing, repo.clone());

    let result = h.restore(uuid, TOKEN).await.expect("restore");

    assert_eq!(result.state, BookmarkState::Active);
    assert_eq!(result.deleted_at, None);
}

#[tokio::test]
async fn given_active_bookmark_when_restore_then_invalid_state() {
    let repo = FakeBookmarkRepository::new();
    let uuid = Uuid::new_v4();
    repo.seed(a_bookmark(uuid, BookmarkState::Active));
    let h = handler(FakeAuth::Allowing, repo);

    let result = h.restore(uuid, TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidState)));
}

#[tokio::test]
async fn given_unknown_uuid_when_restore_then_not_found() {
    let h = handler(FakeAuth::Allowing, FakeBookmarkRepository::new());

    let result = h.restore(Uuid::new_v4(), TOKEN).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

#[tokio::test]
async fn given_unauthenticated_when_restore_then_unauthorized() {
    let repo = FakeBookmarkRepository::new();
    let uuid = Uuid::new_v4();
    repo.seed(a_bookmark(uuid, BookmarkState::Deleted));
    let h = handler(FakeAuth::Denying, repo.clone());

    let result = h.restore(uuid, "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
    assert_eq!(
        repo.bookmark_for(uuid).unwrap().state,
        BookmarkState::Deleted
    );
}
