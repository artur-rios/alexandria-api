//! Unit tests for the UC-33 EditTextFileContentHandler (Testing
//! Specification §6). Each test exercises exactly the handler against
//! trait fakes — no real DB, filesystem, or auth service. Coverage follows
//! §6.3: happy path, wrong file type (AF-01), disk write failure (AF-02),
//! integrity mismatch after retry (AF-03), not-found (AF-04), the
//! unauthorized branch (AF-05), and the soft-deleted-record guard mirrored
//! from `EditMetadataHandler`.

use uuid::Uuid;

use alexandria_core::catalog::commands::edit_content::EditTextFileContentHandler;
use alexandria_core::catalog::model::FileType;
use alexandria_core::errors::DomainError;

use crate::common::{
    deleted_file, existing_file, fixed_clock, now, FakeAuth, FakeCatalogRepository, FakeFilesystem,
};

const TOKEN: &str = "bearer-token";

fn handler(
    auth: FakeAuth,
    repo: FakeCatalogRepository,
    fs: FakeFilesystem,
) -> EditTextFileContentHandler<
    FakeAuth,
    FakeCatalogRepository,
    FakeFilesystem,
    alexandria_core::catalog::clock::FixedClock,
> {
    EditTextFileContentHandler::new(auth, repo, fs, fixed_clock(now()))
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_text_file_when_edited_then_content_written_and_hash_refreshed() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file("/lib/notes.txt", FileType::Text);
    let uuid = file.uuid;
    repo.seed(file);
    let fs = FakeFilesystem::builder()
        .with_text_content("/lib/notes.txt", "old content")
        .build();
    let h = handler(FakeAuth::Allowing, repo.clone(), fs.clone());

    let result = h
        .edit(uuid, "new content".to_string(), TOKEN)
        .await
        .expect("edit");

    assert_eq!(result.uuid, uuid);
    assert_eq!(
        fs.written_content("/lib/notes.txt").as_deref(),
        Some("new content")
    );
    let expected_hash = alexandria_core::catalog::fs::sha256_hex("new content".as_bytes());
    assert_eq!(result.content_hash, Some(expected_hash.clone()));
    let persisted = repo.file_for_uuid(uuid).expect("persisted");
    assert_eq!(persisted.content_hash, Some(expected_hash));
}

// ---------------- Nullable content_hash (Task 3 / FR-FC-09) ----------------

/// A file indexed after Task 3 carries `content_hash: None` — indexing never
/// hashes bytes. Editing it must still succeed and end with the *new*
/// content's hash stored: the handler never reads the pre-edit
/// `content_hash` at all, so starting from `None` is no different from
/// starting from a real value.
#[tokio::test]
async fn given_a_file_with_no_stored_hash_when_content_is_edited_then_the_hash_is_computed_first() {
    let repo = FakeCatalogRepository::new();
    let mut file = existing_file("/lib/notes.txt", FileType::Text);
    file.content_hash = None;
    let uuid = file.uuid;
    repo.seed(file);
    let fs = FakeFilesystem::builder()
        .with_text_content("/lib/notes.txt", "hello")
        .build();
    let h = handler(FakeAuth::Allowing, repo.clone(), fs);

    h.edit(uuid, "goodbye".to_string(), TOKEN)
        .await
        .expect("edit should succeed");

    let file = repo.file_for_uuid(uuid).expect("persisted");
    assert_eq!(
        file.content_hash,
        Some(alexandria_core::catalog::fs::sha256_hex(b"goodbye"))
    );
}

// ---------------- AF-01: invalid input (wrong file type) ----------------

#[tokio::test]
async fn given_non_text_file_when_edited_then_invalid_input() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file("/lib/song.mp3", FileType::Audio);
    let uuid = file.uuid;
    repo.seed(file);
    let fs = FakeFilesystem::builder().build();
    let h = handler(FakeAuth::Allowing, repo, fs);

    let result = h.edit(uuid, "new content".to_string(), TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
}

// ---------------- AF-02: disk write failure ----------------

#[tokio::test]
async fn given_write_failure_when_edited_then_disk_error_and_catalog_unchanged() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file("/lib/notes.txt", FileType::Text);
    let uuid = file.uuid;
    repo.seed(file);
    let mut fs = FakeFilesystem::builder()
        .with_text_content("/lib/notes.txt", "old content")
        .build();
    fs.fail_write_to("/lib/notes.txt");
    let h = handler(FakeAuth::Allowing, repo.clone(), fs);

    let result = h.edit(uuid, "new content".to_string(), TOKEN).await;

    assert!(matches!(result, Err(DomainError::Disk(_))));
    assert_eq!(
        repo.file_for_uuid(uuid).expect("persisted").content_hash,
        Some("preexisting".to_string())
    );
}

// ---------------- AF-03: integrity mismatch ----------------

#[tokio::test]
async fn given_corrupted_write_when_edited_then_integrity_error_after_retry() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file("/lib/notes.txt", FileType::Text);
    let uuid = file.uuid;
    repo.seed(file);
    let mut fs = FakeFilesystem::builder()
        .with_text_content("/lib/notes.txt", "old content")
        .build();
    fs.corrupt_write_to("/lib/notes.txt");
    let h = handler(FakeAuth::Allowing, repo.clone(), fs);

    let result = h.edit(uuid, "new content".to_string(), TOKEN).await;

    assert!(matches!(result, Err(DomainError::Integrity(_))));
    assert_eq!(
        repo.file_for_uuid(uuid).expect("persisted").content_hash,
        Some("preexisting".to_string())
    );
}

// ---------------- AF-04: not found ----------------

#[tokio::test]
async fn given_unknown_uuid_when_edited_then_not_found() {
    let repo = FakeCatalogRepository::new();
    let fs = FakeFilesystem::builder().build();
    let h = handler(FakeAuth::Allowing, repo, fs);

    let result = h
        .edit(Uuid::new_v4(), "new content".to_string(), TOKEN)
        .await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

// ---------------- Soft-deleted record guard ----------------

#[tokio::test]
async fn given_deleted_file_when_edited_then_invalid_state() {
    let repo = FakeCatalogRepository::new();
    let file = deleted_file("/lib/notes.txt", "notes.txt", FileType::Text);
    let uuid = file.uuid;
    repo.seed(file);
    let fs = FakeFilesystem::builder().build();
    let h = handler(FakeAuth::Allowing, repo, fs);

    let result = h.edit(uuid, "new content".to_string(), TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidState)));
}

// ---------------- AF-05: unauthorized ----------------

#[tokio::test]
async fn given_unauthenticated_when_edited_then_unauthorized() {
    let repo = FakeCatalogRepository::new();
    let fs = FakeFilesystem::builder().build();
    let h = handler(FakeAuth::Denying, repo, fs);

    let result = h.edit(Uuid::new_v4(), "new content".to_string(), "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

#[tokio::test]
async fn given_unauthenticated_and_unknown_uuid_when_edited_then_unauthorized_not_not_found() {
    let repo = FakeCatalogRepository::new();
    let fs = FakeFilesystem::builder().build();
    let h = handler(FakeAuth::Denying, repo, fs);

    let result = h.edit(Uuid::new_v4(), "new content".to_string(), "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}
