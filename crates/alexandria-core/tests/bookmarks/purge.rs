//! Unit tests for the UC-19 PurgeBookmarkHandler (Testing Specification §6).
//! Each test exercises exactly the handler against trait fakes — no real DB
//! or auth service. Coverage follows §6.3: happy path, every `AF-xx`
//! alternative flow, the not-deleted state precondition, and the
//! retention-window boundary (inclusive-restorable / strict-purgeable).

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use alexandria_core::bookmarks::commands::purge::PurgeBookmarkHandler;
use alexandria_core::bookmarks::model::{Bookmark, BookmarkState};
use alexandria_core::catalog::clock::FixedClock;
use alexandria_core::errors::DomainError;

use crate::common::{FakeAuth, FakeBookmarkRepository};

const TOKEN: &str = "bearer-token";
const RETENTION_DAYS: u32 = 30;

fn now() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap()
}

fn handler(
    auth: FakeAuth,
    repo: FakeBookmarkRepository,
    retention_days: u32,
) -> PurgeBookmarkHandler<FakeAuth, FakeBookmarkRepository, FixedClock> {
    PurgeBookmarkHandler::new(auth, repo, FixedClock(now()), retention_days)
}

fn deleted_bookmark_at(uuid: Uuid, deleted_at: DateTime<Utc>) -> Bookmark {
    Bookmark {
        uuid,
        url: "https://example.com".to_string(),
        title: "Example".to_string(),
        state: BookmarkState::Deleted,
        deleted_at: Some(deleted_at),
        collection_uuid: None,
    }
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_deleted_bookmark_past_retention_when_purge_then_removed_and_returned() {
    let repo = FakeBookmarkRepository::new();
    let uuid = Uuid::new_v4();
    repo.seed(deleted_bookmark_at(
        uuid,
        now() - Duration::days(i64::from(RETENTION_DAYS)) - Duration::seconds(1),
    ));
    let h = handler(FakeAuth::Allowing, repo.clone(), RETENTION_DAYS);

    let result = h.purge(uuid, TOKEN).await.expect("purge");

    assert_eq!(result.uuid, uuid);
    assert!(repo.bookmark_for(uuid).is_none(), "record removed");
}

// ---------------- Retention boundary ----------------

#[tokio::test]
async fn given_deleted_bookmark_exactly_at_retention_boundary_when_purge_then_invalid_state() {
    let repo = FakeBookmarkRepository::new();
    let uuid = Uuid::new_v4();
    repo.seed(deleted_bookmark_at(
        uuid,
        now() - Duration::days(i64::from(RETENTION_DAYS)),
    ));
    let h = handler(FakeAuth::Allowing, repo.clone(), RETENTION_DAYS);

    let result = h.purge(uuid, TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidState)));
    assert!(repo.bookmark_for(uuid).is_some(), "record kept");
}

#[tokio::test]
async fn given_deleted_bookmark_within_retention_when_purge_then_invalid_state() {
    let repo = FakeBookmarkRepository::new();
    let uuid = Uuid::new_v4();
    repo.seed(deleted_bookmark_at(uuid, now() - Duration::days(1)));
    let h = handler(FakeAuth::Allowing, repo.clone(), RETENTION_DAYS);

    let result = h.purge(uuid, TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidState)));
}

// ---------------- Not-deleted precondition ----------------

#[tokio::test]
async fn given_active_bookmark_when_purge_then_invalid_state() {
    let repo = FakeBookmarkRepository::new();
    let uuid = Uuid::new_v4();
    repo.seed(Bookmark {
        uuid,
        url: "https://example.com".to_string(),
        title: "Example".to_string(),
        state: BookmarkState::Active,
        deleted_at: None,
        collection_uuid: None,
    });
    let h = handler(FakeAuth::Allowing, repo.clone(), RETENTION_DAYS);

    let result = h.purge(uuid, TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidState)));
}

// ---------------- AF-02: bookmark does not exist ----------------

#[tokio::test]
async fn given_unknown_uuid_when_purge_then_not_found() {
    let h = handler(
        FakeAuth::Allowing,
        FakeBookmarkRepository::new(),
        RETENTION_DAYS,
    );

    let result = h.purge(Uuid::new_v4(), TOKEN).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

// ---------------- AF-03: unauthorized ----------------

#[tokio::test]
async fn given_unauthenticated_when_purge_then_unauthorized_and_record_kept() {
    let repo = FakeBookmarkRepository::new();
    let uuid = Uuid::new_v4();
    repo.seed(deleted_bookmark_at(
        uuid,
        now() - Duration::days(i64::from(RETENTION_DAYS)) - Duration::seconds(1),
    ));
    let h = handler(FakeAuth::Denying, repo.clone(), RETENTION_DAYS);

    let result = h.purge(uuid, "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
    assert!(repo.bookmark_for(uuid).is_some(), "record kept");
}
