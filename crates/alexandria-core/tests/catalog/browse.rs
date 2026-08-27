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
use alexandria_core::catalog::repos::CatalogRepository;
use alexandria_core::errors::DomainError;

use crate::common::{deleted_file, existing_file_with_hash, FakeAuth, FakeCatalogRepository};

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
    repo.seed(existing_file_with_hash(
        "/lib/a.mp3",
        "a",
        FileType::Audio,
        "h-a",
    ));
    repo.seed(existing_file_with_hash(
        "/lib/b.md",
        "b",
        FileType::Text,
        "h-b",
    ));
    repo.seed(deleted_file("/lib/c.mp3", "c", FileType::Audio));

    let h = handler(FakeAuth::Allowing, repo.clone());
    let files = h.list(FileFilter::new(), TOKEN).await.expect("list");

    // Default state filter is Active → c.mp3 (deleted) is excluded.
    let names: Vec<&str> = files.iter().map(|f| f.file.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["a", "b"],
        "default list excludes deleted records"
    );
    assert!(files.iter().all(|f| f.file.state == FileState::Active));
}

#[tokio::test]
async fn given_files_when_list_filtered_by_type_then_only_matching_type_returned() {
    let repo = FakeCatalogRepository::new();
    repo.seed(existing_file_with_hash(
        "/lib/a.mp3",
        "a",
        FileType::Audio,
        "h",
    ));
    repo.seed(existing_file_with_hash(
        "/lib/b.mp4",
        "b",
        FileType::Video,
        "h",
    ));
    repo.seed(existing_file_with_hash(
        "/lib/c.md",
        "c",
        FileType::Text,
        "h",
    ));

    let h = handler(FakeAuth::Allowing, repo);
    let filter = FileFilter::new().with_type(FileType::Audio);
    let files = h.list(filter, TOKEN).await.expect("list");

    assert_eq!(files.len(), 1);
    assert_eq!(files[0].file.file_type, FileType::Audio);
}

// issue #116 / FR-FC-12: the listing answers the same FileView shape
// get_by_uuid does, assembled by the repository's batched
// list_filtered_view rather than a per-row detail call.

#[tokio::test]
async fn given_listing_of_one_type_when_list_then_each_row_carries_its_metadata() {
    let repo = FakeCatalogRepository::new();
    let a = existing_file_with_hash("/lib/a.mp3", "a", FileType::Audio, "h");
    let b = existing_file_with_hash("/lib/b.mp3", "b", FileType::Audio, "h");
    let (a_uuid, b_uuid) = (a.uuid, b.uuid);
    repo.seed(a);
    repo.seed(b);
    repo.seed_metadata(
        a_uuid,
        SubtypeMetadata::Audio {
            title: Some("Airbag".into()),
            artist: Some("Radiohead".into()),
            album: None,
            year: None,
            genre: None,
            track: None,
            album_artist: None,
        },
    );
    repo.seed_metadata(
        b_uuid,
        SubtypeMetadata::Audio {
            title: Some("Karma Police".into()),
            artist: Some("Radiohead".into()),
            album: None,
            year: None,
            genre: None,
            track: None,
            album_artist: None,
        },
    );

    let h = handler(FakeAuth::Allowing, repo);
    let filter = FileFilter::new().with_type(FileType::Audio);
    let files = h.list(filter, TOKEN).await.expect("list");

    assert_eq!(files.len(), 2);
    for view in &files {
        match &view.metadata {
            Some(SubtypeMetadata::Audio { artist, .. }) => {
                assert_eq!(artist.as_deref(), Some("Radiohead"));
            }
            other => panic!("expected audio metadata, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn given_mixed_type_listing_when_list_then_each_row_carries_its_own_metadata() {
    let repo = FakeCatalogRepository::new();
    let song = existing_file_with_hash("/lib/song.mp3", "song", FileType::Audio, "h");
    let movie = existing_file_with_hash("/lib/movie.mp4", "movie", FileType::Video, "h");
    let (song_uuid, movie_uuid) = (song.uuid, movie.uuid);
    repo.seed(song);
    repo.seed(movie);
    repo.seed_metadata(
        song_uuid,
        SubtypeMetadata::Audio {
            title: Some("Song".into()),
            artist: None,
            album: None,
            year: None,
            genre: None,
            track: None,
            album_artist: None,
        },
    );
    repo.seed_metadata(
        movie_uuid,
        SubtypeMetadata::Video {
            title: Some("Movie".into()),
            year: None,
            resolution: None,
            media_kind: None,
        },
    );

    let h = handler(FakeAuth::Allowing, repo);
    let files = h
        .list(FileFilter::new().with_state(StateFilter::All), TOKEN)
        .await
        .expect("list");

    assert_eq!(files.len(), 2);
    let song_view = files
        .iter()
        .find(|v| v.file.uuid == song_uuid)
        .expect("song present");
    let movie_view = files
        .iter()
        .find(|v| v.file.uuid == movie_uuid)
        .expect("movie present");
    match &song_view.metadata {
        Some(SubtypeMetadata::Audio { title, .. }) => assert_eq!(title.as_deref(), Some("Song")),
        other => panic!("expected audio metadata, got {other:?}"),
    }
    match &movie_view.metadata {
        Some(SubtypeMetadata::Video { title, .. }) => assert_eq!(title.as_deref(), Some("Movie")),
        other => panic!("expected video metadata, got {other:?}"),
    }
}

#[tokio::test]
async fn given_file_with_no_stored_metadata_when_list_then_metadata_is_none_not_error() {
    let repo = FakeCatalogRepository::new();
    repo.seed(existing_file_with_hash(
        "/lib/a.mp3",
        "a",
        FileType::Audio,
        "h",
    ));

    let h = handler(FakeAuth::Allowing, repo);
    let files = h.list(FileFilter::new(), TOKEN).await.expect("list");

    assert_eq!(files.len(), 1);
    assert!(
        files[0].metadata.is_none(),
        "an absent subtype row carries no metadata rather than failing"
    );
}

#[tokio::test]
async fn given_mixed_files_when_list_state_deleted_then_only_deleted_returned() {
    let repo = FakeCatalogRepository::new();
    repo.seed(existing_file_with_hash(
        "/lib/a.mp3",
        "a",
        FileType::Audio,
        "h",
    ));
    repo.seed(deleted_file("/lib/b.mp3", "b", FileType::Audio));

    let h = handler(FakeAuth::Allowing, repo);
    let filter = FileFilter::new().with_state(StateFilter::Deleted);
    let files = h.list(filter, TOKEN).await.expect("list");

    assert_eq!(files.len(), 1);
    assert_eq!(files[0].file.name, "b");
    assert_eq!(files[0].file.state, FileState::Deleted);
}

#[tokio::test]
async fn given_mixed_files_when_list_state_all_then_both_active_and_deleted_returned() {
    let repo = FakeCatalogRepository::new();
    repo.seed(existing_file_with_hash(
        "/lib/a.mp3",
        "a",
        FileType::Audio,
        "h",
    ));
    repo.seed(deleted_file("/lib/b.mp3", "b", FileType::Audio));

    let h = handler(FakeAuth::Allowing, repo);
    let filter = FileFilter::new().with_state(StateFilter::All);
    let files = h.list(filter, TOKEN).await.expect("list");

    assert_eq!(files.len(), 2, "All returns both active and deleted");
}

#[tokio::test]
async fn given_files_when_list_type_and_state_combined_then_filter_applied() {
    let repo = FakeCatalogRepository::new();
    repo.seed(existing_file_with_hash(
        "/lib/a.mp3",
        "a",
        FileType::Audio,
        "h",
    ));
    repo.seed(deleted_file("/lib/b.mp3", "b", FileType::Audio));
    repo.seed(deleted_file("/lib/c.mp4", "c", FileType::Video));

    let h = handler(FakeAuth::Allowing, repo);
    let filter = FileFilter::new()
        .with_type(FileType::Audio)
        .with_state(StateFilter::Deleted);
    let files = h.list(filter, TOKEN).await.expect("list");

    assert_eq!(files.len(), 1);
    assert_eq!(files[0].file.name, "b");
    assert_eq!(files[0].file.state, FileState::Deleted);
}

// UC-14 / FR-FC-12: collection filter (deferred from UC-03 until UC-14).

#[tokio::test]
async fn given_files_when_list_filtered_by_collection_then_only_linked_files_returned() {
    let repo = FakeCatalogRepository::new();
    let a = existing_file_with_hash("/lib/a.mp3", "a", FileType::Audio, "h");
    let b = existing_file_with_hash("/lib/b.mp3", "b", FileType::Audio, "h");
    let a_uuid = a.uuid;
    repo.seed(a);
    repo.seed(b);

    let collection_uuid = Uuid::new_v4();
    repo.set_collection(a_uuid, collection_uuid)
        .await
        .expect("link a");

    let h = handler(FakeAuth::Allowing, repo);
    let filter = FileFilter::new().with_collection(collection_uuid);
    let files = h.list(filter, TOKEN).await.expect("list");

    assert_eq!(files.len(), 1);
    assert_eq!(files[0].file.uuid, a_uuid);
}

#[tokio::test]
async fn given_unknown_collection_uuid_when_list_filtered_then_empty_list_not_error() {
    let repo = FakeCatalogRepository::new();
    repo.seed(existing_file_with_hash(
        "/lib/a.mp3",
        "a",
        FileType::Audio,
        "h",
    ));

    let h = handler(FakeAuth::Allowing, repo);
    let filter = FileFilter::new().with_collection(Uuid::new_v4());
    let files = h.list(filter, TOKEN).await.expect("list");

    assert!(files.is_empty());
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
        album_artist: None,
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

#[tokio::test]
async fn given_image_with_extracted_dimensions_when_get_by_uuid_then_width_and_height_present() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/photo.jpg", "photo", FileType::Image, "h");
    let uuid = file.uuid;
    repo.seed(file);
    repo.set_image_dimensions(uuid, 800, 600).await.unwrap();

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    assert_eq!(view.width, Some(800));
    assert_eq!(view.height, Some(600));
}

#[tokio::test]
async fn given_image_with_no_extracted_dimensions_when_get_by_uuid_then_width_and_height_none() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/photo.jpg", "photo", FileType::Image, "h");
    let uuid = file.uuid;
    repo.seed(file);

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    assert_eq!(view.width, None);
    assert_eq!(view.height, None);
}

#[tokio::test]
async fn given_non_image_file_when_get_by_uuid_then_width_and_height_none() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/song.mp3", "song", FileType::Audio, "h");
    let uuid = file.uuid;
    repo.seed(file);

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    assert_eq!(view.width, None);
    assert_eq!(view.height, None);
}

#[tokio::test]
async fn given_document_with_extracted_page_count_when_get_by_uuid_then_page_count_present() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/book.pdf", "book", FileType::Document, "h");
    let uuid = file.uuid;
    repo.seed(file);
    repo.set_document_page_count(uuid, 42).await.unwrap();

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    assert_eq!(view.page_count, Some(42));
}

#[tokio::test]
async fn given_document_with_no_extracted_page_count_when_get_by_uuid_then_page_count_none() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/book.epub", "book", FileType::Document, "h");
    let uuid = file.uuid;
    repo.seed(file);

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    assert_eq!(view.page_count, None);
}

#[tokio::test]
async fn given_non_document_file_when_get_by_uuid_then_page_count_none() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/song.mp3", "song", FileType::Audio, "h");
    let uuid = file.uuid;
    repo.seed(file);

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    assert_eq!(view.page_count, None);
}

#[tokio::test]
async fn given_video_with_extracted_duration_when_get_by_uuid_then_duration_present() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/movie.mp4", "movie", FileType::Video, "h");
    let uuid = file.uuid;
    repo.seed(file);
    repo.set_video_duration(uuid, 125.5).await.unwrap();

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    assert_eq!(view.duration_seconds, Some(125.5));
}

#[tokio::test]
async fn given_video_with_no_extracted_duration_when_get_by_uuid_then_duration_none() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/movie.mp4", "movie", FileType::Video, "h");
    let uuid = file.uuid;
    repo.seed(file);

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    assert_eq!(view.duration_seconds, None);
}

#[tokio::test]
async fn given_non_video_file_when_get_by_uuid_then_duration_none() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/song.mp3", "song", FileType::Audio, "h");
    let uuid = file.uuid;
    repo.seed(file);

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    assert_eq!(view.duration_seconds, None);
}

#[tokio::test]
async fn given_comic_with_extracted_page_count_when_get_by_uuid_then_page_count_present() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/issue1.cbz", "issue1", FileType::Comic, "h");
    let uuid = file.uuid;
    repo.seed(file);
    repo.set_comic_page_count(uuid, 24).await.unwrap();

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    assert_eq!(view.comic_page_count, Some(24));
}

#[tokio::test]
async fn given_comic_with_no_extracted_page_count_when_get_by_uuid_then_page_count_none() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/issue1.cbz", "issue1", FileType::Comic, "h");
    let uuid = file.uuid;
    repo.seed(file);

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    assert_eq!(view.comic_page_count, None);
}

#[tokio::test]
async fn given_non_comic_file_when_get_by_uuid_then_comic_page_count_none() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/song.mp3", "song", FileType::Audio, "h");
    let uuid = file.uuid;
    repo.seed(file);

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    assert_eq!(view.comic_page_count, None);
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
