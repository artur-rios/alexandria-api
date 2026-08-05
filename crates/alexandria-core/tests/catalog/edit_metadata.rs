//! Unit tests for the UC-04 EditMetadataHandler (Testing Specification §6).
//! Each test exercises exactly the handler against trait fakes — no real DB,
//! filesystem, or auth service. Coverage follows §6.3: happy path per
//! editable subtype, every `AF-xx` alternative flow, and the text/html
//! non-editable subtype rejection (a form of AF-01).

use uuid::Uuid;

use alexandria_core::auth::AuthService;
use alexandria_core::catalog::commands::edit_metadata::EditMetadataHandler;
use alexandria_core::catalog::model::{
    FileState, FileType, FormatKind, MediaKind, SubtypeMetadata,
};
use alexandria_core::errors::DomainError;

use crate::common::{deleted_file, existing_file_with_hash, FakeAuth, FakeCatalogRepository};

const TOKEN: &str = "bearer-token";

fn handler<A, R>(auth: A, repo: R) -> EditMetadataHandler<A, R>
where
    A: AuthService,
    R: alexandria_core::catalog::repos::CatalogRepository,
{
    EditMetadataHandler::new(auth, repo)
}

/// Builds an active seeded file and returns (uuid, repo_handle, handler).
fn seeded(
    file_type: FileType,
) -> (
    Uuid,
    FakeCatalogRepository,
    EditMetadataHandler<FakeAuth, FakeCatalogRepository>,
) {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/seed", "seed", file_type, "h");
    let uuid = file.uuid;
    repo.seed(file);
    let h = handler(FakeAuth::Allowing, repo.clone());
    (uuid, repo, h)
}

// ---------- Audio (FR-FC-14) ----------

#[tokio::test]
async fn given_active_audio_file_when_edit_audio_metadata_then_metadata_replaced() {
    let (uuid, repo, h) = seeded(FileType::Audio);
    let metadata = SubtypeMetadata::Audio {
        title: Some("Song".into()),
        artist: Some("Artist".into()),
        album: None,
        year: Some(2001),
        genre: Some("Rock".into()),
        track: Some(3),
    };

    let result = h.edit(uuid, metadata.clone(), TOKEN).await.expect("edit");

    assert_eq!(result.file.uuid, uuid);
    assert_eq!(result.file.file_type, FileType::Audio);
    assert_eq!(result.file.state, FileState::Active);
    assert_eq!(
        result.metadata, metadata,
        "handler echoes the written metadata"
    );
    assert_eq!(
        repo.metadata_for(uuid),
        Some(metadata),
        "repo got the metadata"
    );
}

// ---------- Video (FR-FC-15) ----------

#[tokio::test]
async fn given_active_video_file_when_edit_video_metadata_then_metadata_replaced() {
    let (uuid, repo, h) = seeded(FileType::Video);
    let metadata = SubtypeMetadata::Video {
        title: Some("A Film".into()),
        year: Some(1999),
        resolution: Some("1080p".into()),
        media_kind: Some(MediaKind::Movie),
    };

    h.edit(uuid, metadata.clone(), TOKEN).await.expect("edit");

    assert_eq!(repo.metadata_for(uuid), Some(metadata));
}

#[tokio::test]
async fn given_active_video_file_series_kind_when_edit_video_then_series_persisted() {
    let (uuid, repo, h) = seeded(FileType::Video);
    let metadata = SubtypeMetadata::Video {
        title: Some("Show".into()),
        year: None,
        resolution: None,
        media_kind: Some(MediaKind::Series),
    };

    h.edit(uuid, metadata.clone(), TOKEN).await.expect("edit");

    match repo.metadata_for(uuid) {
        Some(SubtypeMetadata::Video { media_kind, .. }) => {
            assert_eq!(media_kind, Some(MediaKind::Series));
        }
        other => panic!("expected video metadata, got {other:?}"),
    }
}

// ---------- Document (FR-FC-16) ----------

#[tokio::test]
async fn given_active_document_file_when_edit_document_metadata_then_metadata_replaced() {
    let (uuid, repo, h) = seeded(FileType::Document);
    let metadata = SubtypeMetadata::Document {
        title: Some("Title".into()),
        author: Some("Author".into()),
        year: Some(2010),
        format_kind: Some(FormatKind::Ebook),
    };

    h.edit(uuid, metadata.clone(), TOKEN).await.expect("edit");

    assert_eq!(repo.metadata_for(uuid), Some(metadata));
}

// ---------- Comic (FR-FC-17) ----------

#[tokio::test]
async fn given_active_comic_file_when_edit_comic_metadata_then_metadata_replaced() {
    let (uuid, repo, h) = seeded(FileType::Comic);
    let metadata = SubtypeMetadata::Comic {
        title: Some("Issue".into()),
        series: Some("Series".into()),
        issue_number: Some(42),
    };

    h.edit(uuid, metadata.clone(), TOKEN).await.expect("edit");

    assert_eq!(repo.metadata_for(uuid), Some(metadata));
}

// ---------- Image (FR-FC-18) ----------

#[tokio::test]
async fn given_active_image_file_when_edit_image_metadata_then_metadata_replaced() {
    let (uuid, repo, h) = seeded(FileType::Image);
    let metadata = SubtypeMetadata::Image {
        title: Some("Pic".into()),
        caption: Some("A caption".into()),
    };

    h.edit(uuid, metadata.clone(), TOKEN).await.expect("edit");

    assert_eq!(repo.metadata_for(uuid), Some(metadata));
}

// ---------- AF-01: fields don't match the file's subtype ----------

#[tokio::test]
async fn given_audio_file_when_edit_with_video_metadata_then_invalid_input() {
    let (uuid, _repo, h) = seeded(FileType::Audio);
    let video = SubtypeMetadata::Video {
        title: Some("x".into()),
        year: None,
        resolution: None,
        media_kind: None,
    };

    let result = h.edit(uuid, video, TOKEN).await;
    assert!(
        matches!(result, Err(DomainError::InvalidInput(_))),
        "variant mismatch must be AF-01 invalid input"
    );
}

#[tokio::test]
async fn given_image_file_when_edit_with_document_metadata_then_invalid_input() {
    let (uuid, _repo, h) = seeded(FileType::Image);
    let doc = SubtypeMetadata::Document {
        title: None,
        author: None,
        year: None,
        format_kind: None,
    };

    let result = h.edit(uuid, doc, TOKEN).await;
    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
}

// ---------- AF-01: file type with no editable subtype (Text / Html) ----------

#[tokio::test]
async fn given_text_file_when_edit_with_any_metadata_then_invalid_input() {
    // Text files have no editable subtype metadata via UC-04; any PATCH is
    // rejected at the handler because no SubtypeMetadata variant matches
    // Text. We pass an Audio body — the type check rejects it.
    let (uuid, _repo, h) = seeded(FileType::Text);
    let audio = SubtypeMetadata::Audio {
        title: None,
        artist: None,
        album: None,
        year: None,
        genre: None,
        track: None,
    };

    let result = h.edit(uuid, audio, TOKEN).await;
    assert!(
        matches!(result, Err(DomainError::InvalidInput(_))),
        "text has no editable subtype metadata"
    );
}

#[tokio::test]
async fn given_html_file_when_edit_with_any_metadata_then_invalid_input() {
    let (uuid, _repo, h) = seeded(FileType::Html);
    let image = SubtypeMetadata::Image {
        title: None,
        caption: None,
    };

    let result = h.edit(uuid, image, TOKEN).await;
    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
}

// ---------- AF-02: file UUID does not exist ----------

#[tokio::test]
async fn given_missing_uuid_when_edit_then_not_found() {
    let repo = FakeCatalogRepository::new();
    let h = handler(FakeAuth::Allowing, repo);
    let uuid = Uuid::new_v4();
    let audio = SubtypeMetadata::Audio {
        title: Some("x".into()),
        artist: None,
        album: None,
        year: None,
        genre: None,
        track: None,
    };

    let result = h.edit(uuid, audio, TOKEN).await;
    assert!(matches!(result, Err(DomainError::NotFound)));
}

// ---------- AF-03: caller not authenticated ----------

#[tokio::test]
async fn given_unauthenticated_when_edit_then_unauthorized() {
    let repo = FakeCatalogRepository::new();
    let h = handler(FakeAuth::Denying, repo);
    let audio = SubtypeMetadata::Audio {
        title: None,
        artist: None,
        album: None,
        year: None,
        genre: None,
        track: None,
    };

    let result = h.edit(Uuid::new_v4(), audio, "").await;
    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

// ---------- AF-04: file is in `deleted` state ----------

#[tokio::test]
async fn given_deleted_file_when_edit_then_invalid_state() {
    let repo = FakeCatalogRepository::new();
    let file = deleted_file("/lib/d", "d", FileType::Audio);
    let uuid = file.uuid;
    repo.seed(file);
    let h = handler(FakeAuth::Allowing, repo);

    let audio = SubtypeMetadata::Audio {
        title: Some("x".into()),
        artist: None,
        album: None,
        year: None,
        genre: None,
        track: None,
    };

    let result = h.edit(uuid, audio, TOKEN).await;
    assert!(
        matches!(result, Err(DomainError::InvalidState)),
        "editing a deleted file's metadata must require restore (UC-07)"
    );
}

// ---------- Postcondition: returned file carries the catalog's file state ----------

#[tokio::test]
async fn given_edit_succeeds_then_returned_file_is_the_cataloged_file() {
    let (uuid, _repo, h) = seeded(FileType::Comic);
    let metadata = SubtypeMetadata::Comic {
        title: Some("T".into()),
        series: None,
        issue_number: Some(1),
    };

    let result = h.edit(uuid, metadata, TOKEN).await.expect("edit");

    assert_eq!(result.file.uuid, uuid);
    assert_eq!(result.file.file_type, FileType::Comic);
    assert_eq!(result.file.state, FileState::Active);
}
