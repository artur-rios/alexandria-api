//! Unit tests for the UC-03 BrowseFilesHandler (Testing Specification §6).
//! Each test exercises exactly the handler against trait fakes — no real DB,
//! filesystem, or auth service. Coverage follows §6.3: happy path for the
//! list and single-file queries, every `AF-xx` alternative flow, and the
//! default-excludes-deleted behavior required by the use case's main-flow
//! step 2.

use uuid::Uuid;

use alexandria_core::auth::AuthService;
use alexandria_core::catalog::model::{
    FileState, FileType, FormatKind, MediaKind, StateFilter, SubtypeMetadata,
};
use alexandria_core::catalog::queries::browse::{BrowseFilesHandler, FileFilter};
use alexandria_core::errors::DomainError;

use crate::common::{
    deleted_file, existing_file_with_hash, FakeAuth, FakeCatalogRepository,
};

const TOKEN: &str = "bearer-token";

fn handler<A, R>(auth: A, repo: R) -> BrowseFilesHandler<A, R>
where
    A: AuthService,
    R: alexandria_core::catalog::repos::CatalogRepository,
{
    BrowseFilesHandler::new(auth, repo)
}

// ---------------------------------------------------------------------------
// Main flow: list (FR-FC-12)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_mixed_files_when_list_default_then_active_only_excludes_deleted() {
    let repo = FakeCatalogRepository::new();
    repo.seed(existing_file_with_hash("/lib/a.mp3", "a", FileType::Audio, "h-a"));
    repo.seed(existing_file_with_hash("/lib/b.md", "b", FileType::Text, "h-b"));
    repo.seed(deleted_file("/lib/c.mp3", "c", FileType::Audio));

    let h = handler(FakeAuth::Allowing, repo.clone());
    let files = h.list(FileFilter::new(), TOKEN).await.expect("list");

    // Default state filter is Active → c.mp3 (deleted) is excluded.
    let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, vec!["a", "b"], "default list excludes deleted records");
    assert!(files.iter().all(|f| f.state == FileState::Active));
}

#[tokio::test]
async fn given_files_when_list_filtered_by_type_then_only_matching_type_returned() {
    let repo = FakeCatalogRepository::new();
    repo.seed(existing_file_with_hash("/lib/a.mp3", "a", FileType::Audio, "h"));
    repo.seed(existing_file_with_hash("/lib/b.mp4", "b", FileType::Video, "h"));
    repo.seed(existing_file_with_hash("/lib/c.md", "c", FileType::Text, "h"));

    let h = handler(FakeAuth::Allowing, repo);
    let filter = FileFilter::new().with_type(FileType::Audio);
    let files = h.list(filter, TOKEN).await.expect("list");

    assert_eq!(files.len(), 1);
    assert_eq!(files[0].file_type, FileType::Audio);
}

#[tokio::test]
async fn given_mixed_files_when_list_state_deleted_then_only_deleted_returned() {
    let repo = FakeCatalogRepository::new();
    repo.seed(existing_file_with_hash("/lib/a.mp3", "a", FileType::Audio, "h"));
    repo.seed(deleted_file("/lib/b.mp3", "b", FileType::Audio));

    let h = handler(FakeAuth::Allowing, repo);
    let filter = FileFilter::new().with_state(StateFilter::Deleted);
    let files = h.list(filter, TOKEN).await.expect("list");

    assert_eq!(files.len(), 1);
    assert_eq!(files[0].name, "b");
    assert_eq!(files[0].state, FileState::Deleted);
}

#[tokio::test]
async fn given_mixed_files_when_list_state_all_then_both_active_and_deleted_returned() {
    let repo = FakeCatalogRepository::new();
    repo.seed(existing_file_with_hash("/lib/a.mp3", "a", FileType::Audio, "h"));
    repo.seed(deleted_file("/lib/b.mp3", "b", FileType::Audio));

    let h = handler(FakeAuth::Allowing, repo);
    let filter = FileFilter::new().with_state(StateFilter::All);
    let files = h.list(filter, TOKEN).await.expect("list");

    assert_eq!(files.len(), 2, "All returns both active and deleted");
}

#[tokio::test]
async fn given_files_when_list_type_and_state_combined_then_filter_applied() {
    let repo = FakeCatalogRepository::new();
    repo.seed(existing_file_with_hash("/lib/a.mp3", "a", FileType::Audio, "h"));
    repo.seed(deleted_file("/lib/b.mp3", "b", FileType::Audio));
    repo.seed(deleted_file("/lib/c.mp4", "c", FileType::Video));

    let h = handler(FakeAuth::Allowing, repo);
    let filter = FileFilter::new()
        .with_type(FileType::Audio)
        .with_state(StateFilter::Deleted);
    let files = h.list(filter, TOKEN).await.expect("list");

    assert_eq!(files.len(), 1);
    assert_eq!(files[0].name, "b");
    assert_eq!(files[0].state, FileState::Deleted);
}

#[tokio::test]
async fn given_no_files_when_list_then_empty_list_returned() {
    let repo = FakeCatalogRepository::new();
    let h = handler(FakeAuth::Allowing, repo);
    let files = h.list(FileFilter::new(), TOKEN).await.expect("list");
    assert!(files.is_empty());
}

// AF-02: caller not authenticated (list)

#[tokio::test]
async fn given_unauthenticated_when_list_then_unauthorized() {
    let repo = FakeCatalogRepository::new();
    let h = handler(FakeAuth::Denying, repo);
    let result = h.list(FileFilter::new(), "").await;
    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

// ---------------------------------------------------------------------------
// Main flow: get_by_uuid (FR-FC-13)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_existing_file_when_get_by_uuid_then_file_view_returned() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/song.mp3", "song", FileType::Audio, "h");
    let uuid = file.uuid;
    repo.seed(file);

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    assert_eq!(view.file.uuid, uuid);
    assert_eq!(view.file.name, "song");
    // No metadata written yet → None.
    assert!(view.metadata.is_none());
}

#[tokio::test]
async fn given_file_with_written_metadata_when_get_by_uuid_then_metadata_echoed() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/song.mp3", "song", FileType::Audio, "h");
    let uuid = file.uuid;
    repo.seed(file);

    // Seed stored metadata as if UC-04 had written it.
    let metadata = SubtypeMetadata::Audio {
        title: Some("Title".into()),
        artist: Some("Artist".into()),
        album: None,
        year: Some(2001),
        genre: None,
        track: None,
    };
    repo.seed_metadata(uuid, metadata.clone());

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    assert_eq!(view.metadata, Some(metadata));
}

#[tokio::test]
async fn given_text_file_when_get_by_uuid_then_metadata_is_none() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/notes.md", "notes", FileType::Text, "h");
    let uuid = file.uuid;
    repo.seed(file);

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    assert_eq!(view.file.file_type, FileType::Text);
    assert!(view.metadata.is_none(), "Text has no SubtypeMetadata");
}

// AF-01: requested UUID does not exist

#[tokio::test]
async fn given_missing_uuid_when_get_by_uuid_then_not_found() {
    let repo = FakeCatalogRepository::new();
    let h = handler(FakeAuth::Allowing, repo);
    let uuid = Uuid::new_v4();

    let result = h.get_by_uuid(uuid, TOKEN).await;
    assert!(matches!(result, Err(DomainError::NotFound)));
}

// AF-02: caller not authenticated (get_by_uuid)

#[tokio::test]
async fn given_unauthenticated_when_get_by_uuid_then_unauthorized() {
    let repo = FakeCatalogRepository::new();
    let h = handler(FakeAuth::Denying, repo);
    let result = h.get_by_uuid(Uuid::new_v4(), "").await;
    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

// Postcondition: get_by_uuid returns a deleted file when it exists

#[tokio::test]
async fn given_deleted_file_when_get_by_uuid_then_file_returned() {
    let repo = FakeCatalogRepository::new();
    let file = deleted_file("/lib/d.mp3", "d", FileType::Audio);
    let uuid = file.uuid;
    repo.seed(file);

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    assert_eq!(view.file.uuid, uuid);
    assert_eq!(view.file.state, FileState::Deleted);
}

// Sanity test that the subtype discriminator round-trips through the handler
// for a video file with a stored mediaKind (FR-FC-15 parity with UC-04).

#[tokio::test]
async fn given_video_file_with_series_kind_when_get_by_uuid_then_series_returned() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/show.mkv", "show", FileType::Video, "h");
    let uuid = file.uuid;
    repo.seed(file);

    let metadata = SubtypeMetadata::Video {
        title: Some("Show".into()),
        year: None,
        resolution: None,
        media_kind: Some(MediaKind::Series),
    };
    repo.seed_metadata(uuid, metadata.clone());

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    match &view.metadata {
        Some(SubtypeMetadata::Video { media_kind, .. }) => {
            assert_eq!(*media_kind, Some(MediaKind::Series));
        }
        other => panic!("expected video metadata, got {other:?}"),
    }
}

// Sanity test that a document's formatKind round-trips through the handler.

#[tokio::test]
async fn given_document_file_with_ebook_kind_when_get_by_uuid_then_ebook_returned() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/book.pdf", "book", FileType::Document, "h");
    let uuid = file.uuid;
    repo.seed(file);

    let metadata = SubtypeMetadata::Document {
        title: Some("Title".into()),
        author: None,
        year: None,
        format_kind: Some(FormatKind::Ebook),
    };
    repo.seed_metadata(uuid, metadata.clone());

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    match &view.metadata {
        Some(SubtypeMetadata::Document { format_kind, .. }) => {
            assert_eq!(*format_kind, Some(FormatKind::Ebook));
        }
        other => panic!("expected document metadata, got {other:?}"),
    }
}