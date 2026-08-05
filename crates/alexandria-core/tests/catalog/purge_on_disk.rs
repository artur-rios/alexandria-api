//! Unit tests for the UC-09 PurgeFileOnDiskHandler (Testing Specification
//! §6). Each test exercises exactly the handler against trait fakes — no real
//! DB, filesystem, or auth service. Coverage follows §6.3: happy path (both
//! `active` and `deleted` records, since UC-09 has no retention gate), every
//! `AF-xx` alternative flow, and the catalog-write-failure branch.

use uuid::Uuid;

use alexandria_core::catalog::commands::purge_on_disk::PurgeFileOnDiskHandler;
use alexandria_core::catalog::model::{FileState, FileType};
use alexandria_core::errors::DomainError;

use crate::common::{deleted_file, existing_file, FakeAuth, FakeCatalogRepository, FakeFilesystem};

const TOKEN: &str = "bearer-token";

fn handler(
    auth: FakeAuth,
    repo: FakeCatalogRepository,
    fs: FakeFilesystem,
) -> PurgeFileOnDiskHandler<FakeAuth, FakeCatalogRepository, FakeFilesystem> {
    PurgeFileOnDiskHandler::new(auth, repo, fs)
}

/// Build a fake filesystem that records `path` as an on-disk entry, mirroring
/// what the indexer would leave on disk for a cataloged file.
fn fs_with_file(path: &str) -> FakeFilesystem {
    let mut fs = FakeFilesystem::default();
    fs.place_disk_file(path);
    fs
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_active_file_when_purge_on_disk_then_record_removed_and_disk_file_deleted() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file("/lib/song.mp3", FileType::Audio);
    let uuid = file.uuid;
    repo.seed(file);
    let fs = fs_with_file("/lib/song.mp3");
    let h = handler(FakeAuth::Allowing, repo.clone(), fs.clone());

    let result = h.purge_on_disk(uuid, TOKEN).await.expect("purge_on_disk");

    assert_eq!(result.file.uuid, uuid);
    assert!(result.disk_file_present);
    assert!(fs.removed("/lib/song.mp3"));
    assert!(repo.file_for_uuid(uuid).is_none());
}

#[tokio::test]
async fn given_deleted_file_when_purge_on_disk_then_record_removed() {
    // No retention gate — the contrast with UC-08: a `deleted` record is
    // purgeable regardless of how long ago it was deleted.
    let repo = FakeCatalogRepository::new();
    let file = deleted_file("/lib/song.mp3", "song.mp3", FileType::Audio);
    let uuid = file.uuid;
    assert_eq!(file.state, FileState::Deleted);
    repo.seed(file);
    let fs = fs_with_file("/lib/song.mp3");
    let h = handler(FakeAuth::Allowing, repo.clone(), fs.clone());

    let result = h.purge_on_disk(uuid, TOKEN).await.expect("purge_on_disk");

    assert!(result.disk_file_present);
    assert!(repo.file_for_uuid(uuid).is_none());
}

// ---------------- AF-01: no on-disk file present ----------------

#[tokio::test]
async fn given_missing_disk_file_when_purge_on_disk_then_record_removed_and_disk_file_absent_reported()
{
    let repo = FakeCatalogRepository::new();
    let file = existing_file("/lib/song.mp3", FileType::Audio);
    let uuid = file.uuid;
    repo.seed(file);
    // No on-disk entry registered for this path.
    let fs = FakeFilesystem::default();
    let h = handler(FakeAuth::Allowing, repo.clone(), fs);

    let result = h.purge_on_disk(uuid, TOKEN).await.expect("purge_on_disk");

    assert!(!result.disk_file_present, "AF-01: no on-disk file was present");
    assert!(repo.file_for_uuid(uuid).is_none(), "record still removed");
}

// ---------------- AF-02: disk delete failure ----------------

#[tokio::test]
async fn given_disk_delete_failure_when_purge_on_disk_then_disk_error_and_record_kept() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file("/lib/song.mp3", FileType::Audio);
    let uuid = file.uuid;
    repo.seed(file);
    let mut fs = fs_with_file("/lib/song.mp3");
    fs.fail_remove_from("/lib/song.mp3");
    let h = handler(FakeAuth::Allowing, repo.clone(), fs.clone());

    let result = h.purge_on_disk(uuid, TOKEN).await;

    assert!(
        matches!(result, Err(DomainError::Disk(_))),
        "a disk delete failure must surface as DomainError::Disk (AF-02)"
    );
    assert!(
        repo.file_for_uuid(uuid).is_some(),
        "catalog row must be kept when the disk delete fails"
    );
    assert_eq!(fs.remove_count(), 0, "no successful removal recorded");
}

// ---------------- AF-03: unknown uuid ----------------

#[tokio::test]
async fn given_unknown_uuid_when_purge_on_disk_then_not_found() {
    let repo = FakeCatalogRepository::new();
    let fs = FakeFilesystem::default();
    let h = handler(FakeAuth::Allowing, repo, fs);
    let uuid = Uuid::new_v4();

    let result = h.purge_on_disk(uuid, TOKEN).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

// ---------------- AF-04: caller not authenticated ----------------

#[tokio::test]
async fn given_unauthenticated_caller_when_purge_on_disk_then_unauthorized() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file("/lib/song.mp3", FileType::Audio);
    let uuid = file.uuid;
    repo.seed(file);
    let fs = fs_with_file("/lib/song.mp3");
    let h = handler(FakeAuth::Denying, repo.clone(), fs.clone());

    let result = h.purge_on_disk(uuid, "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
    assert_eq!(fs.remove_count(), 0, "auth ran before any disk touch");
    assert!(repo.file_for_uuid(uuid).is_some(), "record kept");
}

// ---------------- Residual case: catalog write fails after disk delete ----------------

#[tokio::test]
async fn given_catalog_purge_failure_when_purge_on_disk_then_error_surfaced() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file("/lib/song.mp3", FileType::Audio);
    let uuid = file.uuid;
    repo.seed(file);
    repo.fail_purge(uuid);
    let fs = fs_with_file("/lib/song.mp3");
    let h = handler(FakeAuth::Allowing, repo.clone(), fs.clone());

    let result = h.purge_on_disk(uuid, TOKEN).await;

    assert!(matches!(result, Err(DomainError::Internal(_))));
    // The disk delete already happened and cannot be undone.
    assert!(fs.removed("/lib/song.mp3"));
    // The record was not removed because the fake's purge failed before
    // mutating its map.
    assert!(repo.file_for_uuid(uuid).is_some());
}
