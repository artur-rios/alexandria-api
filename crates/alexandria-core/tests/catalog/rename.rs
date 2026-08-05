//! Unit tests for the UC-05 RenameFileHandler (Testing Specification §6).
//! Each test exercises exactly the handler against trait fakes — no real DB,
//! filesystem, or auth service. Coverage follows §6.3: happy path, every
//! `AF-xx` alternative flow, the deleted-state precondition, and the disk
//! rollback when the catalog write fails after a successful on-disk rename.

use uuid::Uuid;

use alexandria_core::catalog::commands::rename::{validate_file_name, RenameFileHandler};
use alexandria_core::errors::DomainError;

use crate::common::{
    deleted_file, existing_file_with_hash, FakeAuth, FakeCatalogRepository, FakeFilesystem,
};

const TOKEN: &str = "bearer-token";

fn handler(
    auth: FakeAuth,
    repo: FakeCatalogRepository,
    fs: FakeFilesystem,
) -> RenameFileHandler<FakeAuth, FakeCatalogRepository, FakeFilesystem> {
    RenameFileHandler::new(auth, repo, fs)
}

/// Build a fake filesystem that records `path` as an on-disk entry (so the
/// rename succeeds — `path_exists` reports the source path); mirrors what the
/// indexer would leave on disk for a cataloged file.
fn fs_with_file(path: &str) -> FakeFilesystem {
    let mut fs = FakeFilesystem::default();
    fs.place_disk_file(path);
    fs
}

/// Seed an active cataloged file and return (uuid, repo, fs, handler).
fn seeded(
    path: &str,
    name: &str,
) -> (
    Uuid,
    FakeCatalogRepository,
    FakeFilesystem,
    RenameFileHandler<FakeAuth, FakeCatalogRepository, FakeFilesystem>,
) {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash(
        path,
        name,
        alexandria_core::catalog::model::FileType::Audio,
        "h",
    );
    let uuid = file.uuid;
    repo.seed(file);
    let fs = fs_with_file(path);
    let h = handler(FakeAuth::Allowing, repo.clone(), fs.clone());
    (uuid, repo, fs, h)
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_active_file_when_rename_then_disk_moved_catalog_updated_returned_file_carries_new_name_and_path(
) {
    let (uuid, repo, fs, h) = seeded("/lib/song.mp3", "song.mp3");
    let expected_new_path = "/lib/new-name.mp3";

    let result = h
        .rename(uuid, "new-name.mp3".to_string(), TOKEN)
        .await
        .expect("rename");

    assert_eq!(result.uuid, uuid);
    assert_eq!(result.name, "new-name.mp3");
    assert_eq!(result.path, expected_new_path);
    assert!(
        fs.renamed_to("/lib/song.mp3", expected_new_path),
        "on-disk rename recorded"
    );
    assert_eq!(repo.file_for_uuid(uuid).unwrap().name, "new-name.mp3");
    assert_eq!(repo.file_for_uuid(uuid).unwrap().path, expected_new_path);
}

// ---------------- AF-01: invalid name ----------------

#[tokio::test]
async fn given_empty_name_when_rename_then_invalid_input() {
    let (uuid, _repo, _fs, h) = seeded("/lib/song.mp3", "song.mp3");
    let result = h.rename(uuid, "".to_string(), TOKEN).await;
    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
}

#[tokio::test]
async fn given_whitespace_only_name_when_rename_then_invalid_input() {
    let (uuid, _repo, _fs, h) = seeded("/lib/song.mp3", "song.mp3");
    let result = h.rename(uuid, "   ".to_string(), TOKEN).await;
    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
}

#[tokio::test]
async fn given_name_with_path_separator_when_rename_then_invalid_input() {
    let (uuid, _repo, _fs, h) = seeded("/lib/song.mp3", "song.mp3");
    for bad in ["a/b", "a\\b"] {
        let err = h.rename(uuid, bad.to_string(), TOKEN).await;
        assert!(
            matches!(err, Err(DomainError::InvalidInput(_))),
            "name {bad:?} should be rejected"
        );
    }
}

#[tokio::test]
async fn given_dot_or_dot_dot_name_when_rename_then_invalid_input() {
    let (uuid, _repo, _fs, h) = seeded("/lib/song.mp3", "song.mp3");
    for bad in [".", ".."] {
        let err = h.rename(uuid, bad.to_string(), TOKEN).await;
        assert!(matches!(err, Err(DomainError::InvalidInput(_))));
    }
}

#[tokio::test]
async fn given_name_with_reserved_char_when_rename_then_invalid_input() {
    let (uuid, _repo, _fs, h) = seeded("/lib/song.mp3", "song.mp3");
    for bad in ['<', '>', ':', '"', '|', '?', '*'] {
        let name = format!("a{bad}b");
        let err = h.rename(uuid, name, TOKEN).await;
        assert!(
            matches!(err, Err(DomainError::InvalidInput(_))),
            "reserved {bad:?} rejected"
        );
    }
}

#[tokio::test]
async fn given_name_trailing_dot_or_leading_trailing_space_when_rename_then_invalid_input() {
    let (uuid, _repo, _fs, h) = seeded("/lib/song.mp3", "song.mp3");
    for bad in ["name.", "name ", " name", " name "] {
        let err = h.rename(uuid, bad.to_string(), TOKEN).await;
        assert!(
            matches!(err, Err(DomainError::InvalidInput(_))),
            "name {bad:?} should be rejected"
        );
    }
}

#[tokio::test]
async fn given_name_longer_than_255_bytes_when_rename_then_invalid_input() {
    let (uuid, _repo, _fs, h) = seeded("/lib/song.mp3", "song.mp3");
    let name = "a".repeat(256);
    let err = h.rename(uuid, name, TOKEN).await;
    assert!(matches!(err, Err(DomainError::InvalidInput(_))));
}

#[tokio::test]
async fn given_valid_name_with_inner_spaces_and_dots_when_rename_then_succeeds() {
    let (uuid, _repo, _fs, h) = seeded("/lib/song.mp3", "song.mp3");
    let result = h.rename(uuid, "my song v2.mp3".to_string(), TOKEN).await;
    assert!(result.is_ok(), "name with spaces/dots is valid");
    let result = result.unwrap();
    assert_eq!(result.name, "my song v2.mp3");
    assert_eq!(result.path, "/lib/my song v2.mp3");
}

// ---------------- AF-02: disk failure & target-exists ----------------

#[tokio::test]
async fn given_disk_rename_fails_when_rename_then_disk_error_and_catalog_unchanged() {
    let (uuid, repo, mut fs, _h) = seeded("/lib/song.mp3", "song.mp3");
    fs.fail_rename_from("/lib/song.mp3");
    let h = handler(FakeAuth::Allowing, repo.clone(), fs.clone());

    let result = h.rename(uuid, "new-name.mp3".to_string(), TOKEN).await;
    assert!(
        matches!(result, Err(DomainError::Disk(_))),
        "a disk rename failure must surface as DomainError::Disk (AF-02)"
    );
    // Catalog row untouched.
    assert_eq!(repo.file_for_uuid(uuid).unwrap().name, "song.mp3");
    assert_eq!(repo.file_for_uuid(uuid).unwrap().path, "/lib/song.mp3");
    // No rename attempted on disk beyond the failing one.
    assert_eq!(fs.rename_count(), 0);
}

#[tokio::test]
async fn given_target_path_cataloged_for_other_file_when_rename_then_disk_error_no_disk_move() {
    let repo = FakeCatalogRepository::new();
    let a = existing_file_with_hash(
        "/lib/a.mp3",
        "a.mp3",
        alexandria_core::catalog::model::FileType::Audio,
        "h",
    );
    let b = existing_file_with_hash(
        "/lib/b.mp3",
        "b.mp3",
        alexandria_core::catalog::model::FileType::Audio,
        "h",
    );
    let uuid_a = a.uuid;
    repo.seed(a);
    repo.seed(b);
    let fs = fs_with_file("/lib/a.mp3");
    let h = handler(FakeAuth::Allowing, repo, fs.clone());

    // Rename a onto b's path would collide with b's cataloged path.
    let result = h.rename(uuid_a, "b.mp3".to_string(), TOKEN).await;
    assert!(matches!(result, Err(DomainError::Disk(_))));
    assert_eq!(
        fs.rename_count(),
        0,
        "no disk move attempted when the target is cataloged"
    );
}

#[tokio::test]
async fn given_target_path_exists_on_disk_when_rename_then_disk_error_no_disk_move() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash(
        "/lib/song.mp3",
        "song.mp3",
        alexandria_core::catalog::model::FileType::Audio,
        "h",
    );
    let uuid = file.uuid;
    repo.seed(file);
    let mut fs = fs_with_file("/lib/song.mp3");
    // An on-disk entry already sits at the target path (not cataloged).
    fs.place_disk_file("/lib/new-name.mp3");
    let h = handler(FakeAuth::Allowing, repo, fs.clone());

    let result = h.rename(uuid, "new-name.mp3".to_string(), TOKEN).await;
    assert!(matches!(result, Err(DomainError::Disk(_))));
    assert_eq!(
        fs.rename_count(),
        0,
        "no disk move attempted when the target already exists on disk"
    );
}

#[tokio::test]
async fn given_disk_rename_succeeds_but_catalog_fails_when_rename_then_disk_rolled_back_and_catalog_unchanged(
) {
    // Arrange a seeded file and tell the fake repo to refuse `rename_file`
    // for it — the on-disk rename succeeds, then the catalog write fails, so
    // the handler must compensate by moving the on-disk file back to its
    // original path. AF-02's required end state: catalog unchanged, on-disk
    // file untouched.
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash(
        "/lib/song.mp3",
        "song.mp3",
        alexandria_core::catalog::model::FileType::Audio,
        "h",
    );
    let uuid = file.uuid;
    repo.seed(file);
    repo.fail_rename_file(uuid);
    let fs = fs_with_file("/lib/song.mp3");
    let h = handler(FakeAuth::Allowing, repo.clone(), fs.clone());

    let result = h.rename(uuid, "new-name.mp3".to_string(), TOKEN).await;
    assert!(
        matches!(result, Err(DomainError::Internal(_))),
        "the catalog write must surface its failure to the caller"
    );
    // Catalog row untouched (the tx rolled back).
    assert_eq!(repo.file_for_uuid(uuid).unwrap().name, "song.mp3");
    assert_eq!(repo.file_for_uuid(uuid).unwrap().path, "/lib/song.mp3");
    // The on-disk rename was attempted and then rolled back to the original.
    assert!(
        fs.renamed_to("/lib/song.mp3", "/lib/new-name.mp3"),
        "disk rename happened"
    );
    assert!(
        fs.renamed_to("/lib/new-name.mp3", "/lib/song.mp3"),
        "disk rollback happened"
    );
}

// ---------------- AF-03: file UUID does not exist ----------------

#[tokio::test]
async fn given_missing_uuid_when_rename_then_not_found() {
    let repo = FakeCatalogRepository::new();
    let fs = FakeFilesystem::default();
    let h = handler(FakeAuth::Allowing, repo, fs);
    let uuid = Uuid::new_v4();

    let result = h.rename(uuid, "new.mp3".to_string(), TOKEN).await;
    assert!(matches!(result, Err(DomainError::NotFound)));
}

// ---------------- AF-04: caller not authenticated ----------------

#[tokio::test]
async fn given_unauthenticated_when_rename_then_unauthorized_and_no_disk_move() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash(
        "/lib/song.mp3",
        "song.mp3",
        alexandria_core::catalog::model::FileType::Audio,
        "h",
    );
    let uuid = file.uuid;
    repo.seed(file);
    let fs = fs_with_file("/lib/song.mp3");
    let h = handler(FakeAuth::Denying, repo, fs.clone());

    let result = h.rename(uuid, "new.mp3".to_string(), "").await;
    assert!(matches!(result, Err(DomainError::Unauthorized)));
    assert_eq!(fs.rename_count(), 0, "no disk move when unauthenticated");
}

// ---------------- Precondition: deleted state ----------------

#[tokio::test]
async fn given_deleted_file_when_rename_then_invalid_state() {
    let repo = FakeCatalogRepository::new();
    let file = deleted_file(
        "/lib/d.mp3",
        "d.mp3",
        alexandria_core::catalog::model::FileType::Audio,
    );
    let uuid = file.uuid;
    repo.seed(file);
    let fs = fs_with_file("/lib/d.mp3");
    let h = handler(FakeAuth::Allowing, repo, fs);

    let result = h.rename(uuid, "new.mp3".to_string(), TOKEN).await;
    assert!(
        matches!(result, Err(DomainError::InvalidState)),
        "renaming a deleted file must require restore (UC-07)"
    );
}

// ---------------- Same-name no-op ----------------

#[tokio::test]
async fn given_new_name_equals_current_when_rename_then_no_move_no_write_unchanged_file() {
    let (uuid, repo, fs, h) = seeded("/lib/song.mp3", "song.mp3");

    let result = h
        .rename(uuid, "song.mp3".to_string(), TOKEN)
        .await
        .expect("noop");

    assert_eq!(result.name, "song.mp3");
    assert_eq!(result.path, "/lib/song.mp3");
    assert_eq!(fs.rename_count(), 0, "no disk move");
    assert!(!fs.renamed_to("/lib/song.mp3", "/lib/song.mp3"));
    // Catalog row unchanged.
    assert_eq!(repo.file_for_uuid(uuid).unwrap().name, "song.mp3");
    assert_eq!(repo.file_for_uuid(uuid).unwrap().path, "/lib/song.mp3");
}

// ---------------- Validate helper is exercised ----------------

#[test]
fn given_name_with_nul_byte_when_validated_then_invalid_input() {
    let err = validate_file_name("a\0b");
    assert!(matches!(err, Err(DomainError::InvalidInput(_))));
}
