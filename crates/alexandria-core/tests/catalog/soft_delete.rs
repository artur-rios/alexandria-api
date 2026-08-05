//! Unit tests for the UC-06 SoftDeleteFileHandler (Testing Specification §6).
//! Each test exercises exactly the handler against trait fakes — no real DB,
//! filesystem, or auth service. Coverage follows §6.3: happy path, every
//! `AF-xx` alternative flow, the already-deleted state precondition, and the
//! repository-write-failure branch.

use chrono::DateTime;
use uuid::Uuid;

use alexandria_core::catalog::clock::FixedClock;
use alexandria_core::catalog::commands::soft_delete::SoftDeleteFileHandler;
use alexandria_core::catalog::model::FileState;
use alexandria_core::errors::DomainError;

use crate::common::{
    deleted_file, existing_file, fixed_clock, now, FakeAuth, FakeCatalogRepository,
};

const TOKEN: &str = "bearer-token";

fn handler(
    auth: FakeAuth,
    repo: FakeCatalogRepository,
    clock: FixedClock,
) -> SoftDeleteFileHandler<FakeAuth, FakeCatalogRepository, FixedClock> {
    SoftDeleteFileHandler::new(auth, repo, clock)
}

/// Seed an active cataloged file and return (uuid, repo, clock, handler).
fn seeded(
    path: &str,
) -> (
    Uuid,
    FakeCatalogRepository,
    FixedClock,
    SoftDeleteFileHandler<FakeAuth, FakeCatalogRepository, FixedClock>,
) {
    let repo = FakeCatalogRepository::new();
    let file = existing_file(path, alexandria_core::catalog::model::FileType::Audio);
    let uuid = file.uuid;
    repo.seed(file);
    let clock = fixed_clock(now());
    let h = handler(FakeAuth::Allowing, repo.clone(), clock);
    (uuid, repo, clock, h)
}

/// Seed an already-soft-deleted cataloged file (UC-06 invalid-state branch /
/// the precondition for UC-07 restore).
fn seeded_deleted(
    path: &str,
) -> (
    Uuid,
    FakeCatalogRepository,
    FixedClock,
    SoftDeleteFileHandler<FakeAuth, FakeCatalogRepository, FixedClock>,
) {
    let repo = FakeCatalogRepository::new();
    let file = deleted_file(
        path,
        "seedy",
        alexandria_core::catalog::model::FileType::Audio,
    );
    let uuid = file.uuid;
    repo.seed(file);
    let clock = fixed_clock(now());
    let h = handler(FakeAuth::Allowing, repo.clone(), clock);
    (uuid, repo, clock, h)
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_active_file_when_soft_delete_then_state_deleted_deleted_at_set_and_returned() {
    let (uuid, repo, clock, h) = seeded("/lib/song.mp3");

    let result = h.soft_delete(uuid, TOKEN).await.expect("soft_delete");

    assert_eq!(result.uuid, uuid);
    assert_eq!(result.state, FileState::Deleted);
    assert_eq!(result.deleted_at, Some(clock.0));
    let persisted = repo.file_for_uuid(uuid).unwrap();
    assert_eq!(persisted.state, FileState::Deleted);
    assert_eq!(persisted.deleted_at, Some(clock.0));
}

#[tokio::test]
async fn given_active_file_when_soft_delete_then_clock_value_taken_as_deleted_at() {
    // A distinct, obviously-different timestamp confirms the handler uses the
    // clock rather than (e.g.) `Utc::now()` or the file's `indexed_at`.
    let stamp = DateTime::from_timestamp(1_700_000_500, 0).unwrap();
    let (uuid, repo, _clock, _h) = seeded("/lib/song.mp3");
    let h = SoftDeleteFileHandler::new(FakeAuth::Allowing, repo.clone(), fixed_clock(stamp));

    let result = h.soft_delete(uuid, TOKEN).await.expect("soft_delete");

    assert_eq!(result.deleted_at, Some(stamp));
    assert_eq!(repo.file_for_uuid(uuid).unwrap().deleted_at, Some(stamp));
}

// ---------------- AF-01: not found ----------------

#[tokio::test]
async fn given_missing_uuid_when_soft_delete_then_not_found() {
    let (_uuid, _repo, _clock, h) = seeded("/lib/song.mp3");
    let other_uuid = uuid::Uuid::new_v4();

    let result = h.soft_delete(other_uuid, TOKEN).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

// ---------------- AF-02: unauthorized ----------------

#[tokio::test]
async fn given_unauthenticated_when_soft_delete_then_unauthorized_and_no_state_change() {
    let (uuid, repo, _clock, _h) = seeded("/lib/song.mp3");
    let h = handler(FakeAuth::Denying, repo.clone(), fixed_clock(now()));

    let result = h.soft_delete(uuid, "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
    let persisted = repo.file_for_uuid(uuid).unwrap();
    assert_eq!(persisted.state, FileState::Active);
    assert_eq!(persisted.deleted_at, None);
}

// ---------------- Already-deleted precondition (InvalidState) ----------------

#[tokio::test]
async fn given_deleted_file_when_soft_delete_then_invalid_state() {
    let (uuid, repo, _clock, h) = seeded_deleted("/lib/song.mp3");

    let result = h.soft_delete(uuid, TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidState)));
    let persisted = repo.file_for_uuid(uuid).expect("file present");
    // The row is unchanged: still deleted at its original deleted_at (the
    // soft-delete did not "re-stamp" the retention window).
    assert_eq!(persisted.state, FileState::Deleted);
    assert!(persisted.deleted_at.is_some());
    assert_eq!(
        persisted.deleted_at,
        repo.file_for_uuid(uuid).unwrap().deleted_at
    );
}

// ---------------- Repository write failure ----------------

#[tokio::test]
async fn given_soft_delete_when_repo_write_fails_then_error_propagated_and_state_unchanged() {
    let (uuid, repo, _clock, _h) = seeded("/lib/song.mp3");
    repo.fail_soft_delete(uuid);
    let h = handler(FakeAuth::Allowing, repo.clone(), fixed_clock(now()));

    let result = h.soft_delete(uuid, TOKEN).await;

    assert!(matches!(result, Err(DomainError::Internal(_))));
    assert_eq!(repo.file_for_uuid(uuid).unwrap().state, FileState::Active);
    assert_eq!(repo.file_for_uuid(uuid).unwrap().deleted_at, None);
}
