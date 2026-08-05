//! Unit tests for the UC-08 PurgeFileHandler (Testing Specification §6).
//! Each test exercises exactly the handler against trait fakes — no real DB,
//! filesystem, or auth service. Coverage follows §6.3: happy path, every
//! `AF-xx` alternative flow, the not-deleted state precondition, the
//! retention-window boundary (inclusive-restorable / strict-purgeable), and
//! the repository-write-failure branch.

use chrono::Duration;
use uuid::Uuid;

use alexandria_core::catalog::clock::FixedClock;
use alexandria_core::catalog::commands::purge::PurgeFileHandler;
use alexandria_core::catalog::model::FileState;
use alexandria_core::errors::DomainError;

use crate::common::{
    deleted_file_at, existing_file, fixed_clock, now, FakeAuth, FakeCatalogRepository,
};

const TOKEN: &str = "bearer-token";
const RETENTION_DAYS: u32 = 30;
const RETENTION: Duration = Duration::seconds((RETENTION_DAYS as i64) * 86_400);

fn handler(
    auth: FakeAuth,
    repo: FakeCatalogRepository,
    clock: FixedClock,
    retention_days: u32,
) -> PurgeFileHandler<FakeAuth, FakeCatalogRepository, FixedClock> {
    PurgeFileHandler::new(auth, repo, clock, retention_days)
}

/// Seed a `deleted` file whose `deleted_at` is `now - elapsed` and return
/// `(uuid, repo, clock, handler)`. `elapsed` controls the retention-window
/// position used by every boundary test below.
fn seeded_deleted_at(
    path: &str,
    elapsed: Duration,
) -> (
    Uuid,
    FakeCatalogRepository,
    FixedClock,
    PurgeFileHandler<FakeAuth, FakeCatalogRepository, FixedClock>,
) {
    let deleted_at = now() - elapsed;
    let repo = FakeCatalogRepository::new();
    let file = deleted_file_at(
        path,
        "seedy",
        alexandria_core::catalog::model::FileType::Audio,
        deleted_at,
    );
    let uuid = file.uuid;
    repo.seed(file);
    let clock = fixed_clock(now());
    let h = handler(FakeAuth::Allowing, repo.clone(), clock, RETENTION_DAYS);
    (uuid, repo, clock, h)
}

// Well-known offsets against `now()` for the 30-day retention window:
//   within-retention   now - 2_591_000s   (~30 days minus 1000s)
//   exactly boundary   now - 2_592_000s   (exactly 30 days — inclusive-restorable)
//   one-past           now - 2_592_001s   (one second past — purgeable)
//   comfortably past   now - 5_184_000s   (2x retention — the happy-path fixture)
// (`RETENTION` is defined alongside `RETENTION_DAYS` near the top of this
// file — the seconds-literal form `30 * 86_400` is const-eval friendly.)

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_deleted_file_past_retention_when_purge_then_record_removed_and_file_returned() {
    let (uuid, repo, _clock, h) = seeded_deleted_at("/lib/song.mp3", RETENTION * 2);

    let result = h.purge(uuid, TOKEN).await.expect("purge");

    assert_eq!(result.uuid, uuid);
    assert_eq!(result.state, FileState::Deleted);
    assert!(repo.file_for_uuid(uuid).is_none());
}

// ---------------- AF-01: retention boundary ----------------

#[tokio::test]
async fn given_deleted_file_exactly_on_retention_boundary_when_purge_then_invalid_state_and_record_kept(
) {
    // Boundary is inclusive-restorable: exactly retention_days ago is still
    // restorable and not yet purgeable (the exact complement of UC-07).
    let (uuid, repo, _clock, h) = seeded_deleted_at("/lib/song.mp3", RETENTION);

    let result = h.purge(uuid, TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidState)));
    let persisted = repo.file_for_uuid(uuid).unwrap();
    assert_eq!(persisted.state, FileState::Deleted);
    assert!(persisted.deleted_at.is_some());
}

#[tokio::test]
async fn given_deleted_file_one_second_past_retention_when_purge_then_record_removed() {
    let (uuid, repo, _clock, h) =
        seeded_deleted_at("/lib/song.mp3", RETENTION + Duration::seconds(1));

    let result = h.purge(uuid, TOKEN).await;

    assert!(
        result.is_ok(),
        "one-second-past-retention purge must succeed: {:?}",
        result
    );
    assert!(repo.file_for_uuid(uuid).is_none());
}

#[tokio::test]
async fn given_deleted_file_within_retention_when_purge_then_invalid_state_and_record_kept() {
    let (uuid, repo, _clock, h) =
        seeded_deleted_at("/lib/song.mp3", RETENTION - Duration::seconds(1000));

    let result = h.purge(uuid, TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidState)));
    let persisted = repo.file_for_uuid(uuid).unwrap();
    assert_eq!(persisted.state, FileState::Deleted);
    assert!(persisted.deleted_at.is_some());
}

// ---------------- AF-01: active file ----------------

#[tokio::test]
async fn given_active_file_when_purge_then_invalid_state_and_record_kept() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file(
        "/lib/song.mp3",
        alexandria_core::catalog::model::FileType::Audio,
    );
    let uuid = file.uuid;
    repo.seed(file);
    let h = handler(
        FakeAuth::Allowing,
        repo.clone(),
        fixed_clock(now()),
        RETENTION_DAYS,
    );

    let result = h.purge(uuid, TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidState)));
    let persisted = repo.file_for_uuid(uuid).unwrap();
    assert_eq!(persisted.state, FileState::Active);
}

// ---------------- AF-02: not found ----------------

#[tokio::test]
async fn given_missing_uuid_when_purge_then_not_found() {
    let (_uuid, _repo, _clock, h) =
        seeded_deleted_at("/lib/song.mp3", RETENTION + Duration::seconds(1));
    let other_uuid = uuid::Uuid::new_v4();

    let result = h.purge(other_uuid, TOKEN).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

// ---------------- AF-03: unauthorized ----------------

#[tokio::test]
async fn given_unauthenticated_when_purge_then_unauthorized_and_record_kept() {
    let (uuid, repo, _clock, _h) =
        seeded_deleted_at("/lib/song.mp3", RETENTION + Duration::seconds(1));
    let h = handler(
        FakeAuth::Denying,
        repo.clone(),
        fixed_clock(now()),
        RETENTION_DAYS,
    );

    let result = h.purge(uuid, "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
    let persisted = repo.file_for_uuid(uuid).unwrap();
    assert_eq!(persisted.state, FileState::Deleted);
    assert!(persisted.deleted_at.is_some());
}

// ---------------- Repository write failure ----------------

#[tokio::test]
async fn given_purge_when_repo_write_fails_then_error_propagated_and_record_kept() {
    let (uuid, repo, _clock, _h) =
        seeded_deleted_at("/lib/song.mp3", RETENTION + Duration::seconds(1));
    repo.fail_purge(uuid);
    let h = handler(
        FakeAuth::Allowing,
        repo.clone(),
        fixed_clock(now()),
        RETENTION_DAYS,
    );

    let result = h.purge(uuid, TOKEN).await;

    assert!(matches!(result, Err(DomainError::Internal(_))));
    let persisted = repo.file_for_uuid(uuid).unwrap();
    assert_eq!(persisted.state, FileState::Deleted);
    assert!(persisted.deleted_at.is_some());
}
