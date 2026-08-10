//! Unit tests for the UC-32 ReadTextFileContentHandler (Testing
//! Specification §6). Each test exercises exactly the handler against
//! trait fakes — no real DB, filesystem, or auth service. Coverage follows
//! §6.3: happy path, wrong file type (AF-01), disk read failure (AF-02),
//! not-found (AF-03), the unauthorized branch (AF-04), and the
//! soft-deleted-record guard the use case's `active` precondition calls for.

use uuid::Uuid;

use alexandria_core::catalog::model::FileType;
use alexandria_core::catalog::queries::read_content::ReadTextFileContentHandler;
use alexandria_core::errors::DomainError;

use crate::common::{deleted_file, existing_file, FakeAuth, FakeCatalogRepository, FakeFilesystem};

const TOKEN: &str = "bearer-token";

fn handler(
    auth: FakeAuth,
    repo: FakeCatalogRepository,
    fs: FakeFilesystem,
) -> ReadTextFileContentHandler<FakeAuth, FakeCatalogRepository, FakeFilesystem> {
    ReadTextFileContentHandler::new(auth, repo, fs)
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_text_file_when_read_then_content_returned() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file("/lib/notes.txt", FileType::Text);
    let uuid = file.uuid;
    repo.seed(file);
    let fs = FakeFilesystem::builder()
        .with_text_content("/lib/notes.txt", "hello world")
        .build();
    let h = handler(FakeAuth::Allowing, repo, fs);

    let result = h.read(uuid, TOKEN).await.expect("read");

    assert_eq!(result.uuid, uuid);
    assert_eq!(result.content, "hello world");
}

// ---------------- AF-01: invalid input (wrong file type) ----------------

#[tokio::test]
async fn given_non_text_file_when_read_then_invalid_input() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file("/lib/song.mp3", FileType::Audio);
    let uuid = file.uuid;
    repo.seed(file);
    let fs = FakeFilesystem::builder().build();
    let h = handler(FakeAuth::Allowing, repo, fs);

    let result = h.read(uuid, TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
}

// ---------------- AF-02: disk error ----------------

#[tokio::test]
async fn given_unreadable_file_when_read_then_disk_error() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file("/lib/notes.txt", FileType::Text);
    let uuid = file.uuid;
    repo.seed(file);
    let fs = FakeFilesystem::builder()
        .with_unreadable_file("/lib", "/lib/notes.txt", "notes.txt")
        .build();
    let h = handler(FakeAuth::Allowing, repo, fs);

    let result = h.read(uuid, TOKEN).await;

    assert!(matches!(result, Err(DomainError::Disk(_))));
}

// ---------------- AF-03: not found ----------------

#[tokio::test]
async fn given_unknown_uuid_when_read_then_not_found() {
    let repo = FakeCatalogRepository::new();
    let fs = FakeFilesystem::builder().build();
    let h = handler(FakeAuth::Allowing, repo, fs);

    let result = h.read(Uuid::new_v4(), TOKEN).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

// ---------------- Soft-deleted record guard ----------------

/// UC-32's precondition names an `active` TextFile. A soft-deleted record is
/// rejected with `InvalidState` (restore first via UC-07) — the same guard
/// UC-33 and UC-04 apply, so the three cannot disagree about what a deleted
/// record permits.
#[tokio::test]
async fn given_deleted_file_when_read_then_invalid_state() {
    let repo = FakeCatalogRepository::new();
    let file = deleted_file("/lib/notes.txt", "notes.txt", FileType::Text);
    let uuid = file.uuid;
    repo.seed(file);
    let fs = FakeFilesystem::builder()
        .with_text_content("/lib/notes.txt", "hello world")
        .build();
    let h = handler(FakeAuth::Allowing, repo, fs);

    let result = h.read(uuid, TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidState)));
}

// ---------------- AF-04: unauthorized ----------------

#[tokio::test]
async fn given_unauthenticated_when_read_then_unauthorized() {
    let repo = FakeCatalogRepository::new();
    let fs = FakeFilesystem::builder().build();
    let h = handler(FakeAuth::Denying, repo, fs);

    let result = h.read(Uuid::new_v4(), "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

#[tokio::test]
async fn given_unauthenticated_and_unknown_uuid_when_read_then_unauthorized_not_not_found() {
    let repo = FakeCatalogRepository::new();
    let fs = FakeFilesystem::builder().build();
    let h = handler(FakeAuth::Denying, repo, fs);

    let result = h.read(Uuid::new_v4(), "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}
