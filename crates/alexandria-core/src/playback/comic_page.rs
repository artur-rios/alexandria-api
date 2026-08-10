//! UC-39 — Read a comic book page (FR-MP-04).

use serde::Serialize;
use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::comic_tags::is_page_entry;
use crate::catalog::model::FileType;
use crate::catalog::repos::CatalogRepository;
use crate::errors::DomainError;
use crate::playback::mime::mime_for_path;
use crate::playback::resolve_playable;

/// One page of a comic archive. `bytes` are the archive entry's own bytes,
/// undecoded and unmodified (FR-MP-03) — a CBZ page is already a JPEG or
/// PNG, so there is nothing to convert.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComicPage {
    pub uuid: Uuid,
    pub page: u32,
    pub page_count: u32,
    pub mime_type: String,
    #[serde(skip)]
    pub bytes: Vec<u8>,
}

/// Comic archive port. Unit tests substitute a fake; the real
/// implementation reads a ZIP.
#[allow(async_fn_in_trait)]
pub trait ComicArchive: Send + Sync {
    /// Names of the archive entries that count as pages, in whatever order
    /// the archive stores them. The handler sorts.
    async fn page_names(&self, path: &str) -> Result<Vec<String>, DomainError>;
    /// The raw bytes of one entry.
    async fn read_entry(&self, path: &str, entry: &str) -> Result<Vec<u8>, DomainError>;
}

/// Real `ComicArchive`, reading CBZ (ZIP) on the blocking pool.
#[derive(Clone, Copy)]
pub struct ZipComicArchive;

impl ComicArchive for ZipComicArchive {
    async fn page_names(&self, path: &str) -> Result<Vec<String>, DomainError> {
        let owned = path.to_string();
        let handle = tokio::task::spawn_blocking(move || {
            let file = std::fs::File::open(&owned)
                .map_err(|e| DomainError::disk(format!("cannot open {owned}: {e}")))?;
            let mut archive = zip::ZipArchive::new(file)
                .map_err(|e| DomainError::disk(format!("cannot read {owned}: {e}")))?;
            let mut names = Vec::new();
            for i in 0..archive.len() {
                let entry = archive
                    .by_index(i)
                    .map_err(|e| DomainError::disk(format!("cannot read entry {i}: {e}")))?;
                let name = entry.name().to_string();
                if is_page_entry(&name) {
                    names.push(name);
                }
            }
            Ok(names)
        });
        match handle.await {
            Ok(result) => result,
            Err(err) => Err(DomainError::internal(format!("archive task failed: {err}"))),
        }
    }

    async fn read_entry(&self, path: &str, entry: &str) -> Result<Vec<u8>, DomainError> {
        let owned = path.to_string();
        let wanted = entry.to_string();
        let handle = tokio::task::spawn_blocking(move || {
            let file = std::fs::File::open(&owned)
                .map_err(|e| DomainError::disk(format!("cannot open {owned}: {e}")))?;
            let mut archive = zip::ZipArchive::new(file)
                .map_err(|e| DomainError::disk(format!("cannot read {owned}: {e}")))?;
            let mut zip_entry = archive
                .by_name(&wanted)
                .map_err(|e| DomainError::disk(format!("cannot read entry {wanted}: {e}")))?;
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut zip_entry, &mut bytes)
                .map_err(|e| DomainError::disk(format!("cannot read entry {wanted}: {e}")))?;
            Ok(bytes)
        });
        match handle.await {
            Ok(result) => result,
            Err(err) => Err(DomainError::internal(format!("archive task failed: {err}"))),
        }
    }
}

/// UC-39 — return page `page` (1-based) of a CBZ ComicBook.
pub struct ComicPageHandler<A, R, C> {
    auth: A,
    repo: R,
    archive: C,
}

impl<A, R, C> ComicPageHandler<A, R, C>
where
    A: AuthService,
    R: CatalogRepository,
    C: ComicArchive,
{
    pub fn new(auth: A, repo: R, archive: C) -> Self {
        Self {
            auth,
            repo,
            archive,
        }
    }

    pub async fn read_page(
        &self,
        uuid: Uuid,
        page: u32,
        token: &str,
    ) -> Result<ComicPage, DomainError> {
        let file = resolve_playable(&self.auth, &self.repo, uuid, token).await?;

        if file.file_type != FileType::Comic {
            return Err(DomainError::InvalidInput(format!(
                "file {uuid} is not a comic book"
            )));
        }

        // CBR is RAR: proprietary, no viable pure-Rust reader. The same
        // graceful-degradation line `comic_tags.rs` already draws — except
        // here the caller asked for something specific, so it is told the
        // format is unsupported rather than silently getting nothing.
        if !file.path.to_ascii_lowercase().ends_with(".cbz") {
            return Err(DomainError::InvalidInput(format!(
                "comic {uuid} is not a CBZ archive; page extraction supports CBZ only"
            )));
        }

        let mut names = self.archive.page_names(&file.path).await?;

        // Archive-storage order is not page order — nothing obliges a writer
        // to store entries in sequence. Sort case-insensitively by name,
        // which is what comic readers conventionally do and what the
        // zero-padded filenames CBZ archives use are designed for.
        names.sort_by_key(|name| name.to_ascii_lowercase());

        let page_count = names.len() as u32;
        if page == 0 || page > page_count {
            return Err(DomainError::InvalidInput(format!(
                "page {page} is out of range; comic {uuid} has {page_count} pages"
            )));
        }

        let entry = &names[(page - 1) as usize];
        let bytes = self.archive.read_entry(&file.path, entry).await?;

        Ok(ComicPage {
            uuid: file.uuid,
            page,
            page_count,
            mime_type: mime_for_path(entry).to_string(),
            bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Principal;
    use crate::catalog::model::{File, FileState, NewFile, StateFilter, SubtypeMetadata};
    use crate::config::AuthMode;
    use chrono::{DateTime, Utc};

    /// Auth fake: accepts any token. Mirrors `playback::source`'s `FakeAuth`
    /// shape — the real `AuthService` trait returns a `Principal`, not `()`.
    #[derive(Clone)]
    struct FakeAuth;

    impl AuthService for FakeAuth {
        async fn authenticate(&self, _token: &str) -> Result<Principal, DomainError> {
            Ok(Principal {
                user_id: "owner".to_string(),
            })
        }

        fn mode(&self) -> AuthMode {
            AuthMode::External
        }
    }

    /// Archive fake: entries deliberately supplied out of order, to prove
    /// the handler sorts rather than trusting archive order.
    #[derive(Clone)]
    struct FakeArchive;

    impl ComicArchive for FakeArchive {
        async fn page_names(&self, _path: &str) -> Result<Vec<String>, DomainError> {
            Ok(vec![
                "page003.jpg".to_string(),
                "page001.jpg".to_string(),
                "page002.png".to_string(),
            ])
        }

        async fn read_entry(&self, _path: &str, entry: &str) -> Result<Vec<u8>, DomainError> {
            Ok(entry.as_bytes().to_vec())
        }
    }

    /// Catalog repo fake returning one canned file. Mirrors `playback::mod`'s
    /// `FakeRepo`: exactly one `CatalogRepository` method is exercised
    /// (`find_by_uuid`), every other method is `unimplemented!()` so an
    /// accidental extra call fails the test loudly.
    #[derive(Clone)]
    struct FakeRepo {
        file: Option<File>,
    }

    impl FakeRepo {
        fn with_file(file: File) -> Self {
            Self { file: Some(file) }
        }
    }

    impl CatalogRepository for FakeRepo {
        async fn find_by_path(&self, _path: &str) -> Result<Option<File>, DomainError> {
            unimplemented!()
        }

        async fn find_by_uuid(&self, _uuid: Uuid) -> Result<Option<File>, DomainError> {
            Ok(self.file.clone())
        }

        async fn insert_file(&self, _new_file: NewFile) -> Result<File, DomainError> {
            unimplemented!()
        }

        async fn list_all(&self) -> Result<Vec<File>, DomainError> {
            unimplemented!()
        }

        async fn refresh_hash(
            &self,
            _path: &str,
            _content_hash: &str,
            _indexed_at: DateTime<Utc>,
        ) -> Result<(), DomainError> {
            unimplemented!()
        }

        async fn mark_missing(
            &self,
            _path: &str,
            _missing_at: DateTime<Utc>,
        ) -> Result<(), DomainError> {
            unimplemented!()
        }

        async fn update_metadata(
            &self,
            _uuid: Uuid,
            _metadata: &SubtypeMetadata,
        ) -> Result<(), DomainError> {
            unimplemented!()
        }

        async fn list_filtered(
            &self,
            _file_type: Option<FileType>,
            _state: StateFilter,
            _collection_uuid: Option<Uuid>,
        ) -> Result<Vec<File>, DomainError> {
            unimplemented!()
        }

        async fn find_metadata_by_uuid(
            &self,
            _uuid: Uuid,
        ) -> Result<Option<SubtypeMetadata>, DomainError> {
            unimplemented!()
        }

        async fn set_image_dimensions(
            &self,
            _uuid: Uuid,
            _width: i64,
            _height: i64,
        ) -> Result<(), DomainError> {
            unimplemented!()
        }

        async fn find_image_dimensions(
            &self,
            _uuid: Uuid,
        ) -> Result<Option<(i64, i64)>, DomainError> {
            unimplemented!()
        }

        async fn set_document_page_count(
            &self,
            _uuid: Uuid,
            _page_count: i64,
        ) -> Result<(), DomainError> {
            unimplemented!()
        }

        async fn find_document_page_count(&self, _uuid: Uuid) -> Result<Option<i64>, DomainError> {
            unimplemented!()
        }

        async fn set_video_duration(
            &self,
            _uuid: Uuid,
            _duration_seconds: f64,
        ) -> Result<(), DomainError> {
            unimplemented!()
        }

        async fn find_video_duration(&self, _uuid: Uuid) -> Result<Option<f64>, DomainError> {
            unimplemented!()
        }

        async fn set_comic_page_count(
            &self,
            _uuid: Uuid,
            _page_count: i64,
        ) -> Result<(), DomainError> {
            unimplemented!()
        }

        async fn find_comic_page_count(&self, _uuid: Uuid) -> Result<Option<i64>, DomainError> {
            unimplemented!()
        }

        async fn rename_file(
            &self,
            _uuid: Uuid,
            _new_name: &str,
            _new_path: &str,
        ) -> Result<File, DomainError> {
            unimplemented!()
        }

        async fn soft_delete(
            &self,
            _uuid: Uuid,
            _deleted_at: DateTime<Utc>,
        ) -> Result<File, DomainError> {
            unimplemented!()
        }

        async fn restore(&self, _uuid: Uuid) -> Result<File, DomainError> {
            unimplemented!()
        }

        async fn purge(&self, _uuid: Uuid) -> Result<(), DomainError> {
            unimplemented!()
        }

        async fn set_collection(
            &self,
            _uuid: Uuid,
            _collection_uuid: Uuid,
        ) -> Result<(), DomainError> {
            unimplemented!()
        }

        async fn clear_collection(
            &self,
            _uuid: Uuid,
            _collection_uuid: Uuid,
        ) -> Result<(), DomainError> {
            unimplemented!()
        }
    }

    fn a_file(path: &str, file_type: FileType) -> File {
        File {
            uuid: Uuid::nil(),
            path: path.to_string(),
            name: path
                .rsplit_once('/')
                .map_or(path, |(_, name)| name)
                .to_string(),
            file_type,
            content_hash: "abc".to_string(),
            state: FileState::Active,
            deleted_at: None,
            indexed_at: Utc::now(),
            missing_at: None,
        }
    }

    #[tokio::test]
    async fn given_cbz_when_page_requested_then_pages_are_lexicographically_ordered() {
        // Arrange — archive order is 3, 1, 2; page order must be 1, 2, 3.
        let repo = FakeRepo::with_file(a_file("/lib/issue.cbz", FileType::Comic));
        let handler = ComicPageHandler::new(FakeAuth, repo, FakeArchive);

        // Act
        let page = handler
            .read_page(Uuid::nil(), 1, "t")
            .await
            .expect("page 1");

        // Assert
        assert_eq!(page.bytes, b"page001.jpg".to_vec());
        assert_eq!(page.mime_type, "image/jpeg");
        assert_eq!(page.page_count, 3);
    }

    #[tokio::test]
    async fn given_cbz_when_last_page_requested_then_its_own_mime_returned() {
        // Arrange — page 2 is a PNG; the MIME comes from the entry, not the
        // archive.
        let repo = FakeRepo::with_file(a_file("/lib/issue.cbz", FileType::Comic));
        let handler = ComicPageHandler::new(FakeAuth, repo, FakeArchive);

        // Act
        let page = handler
            .read_page(Uuid::nil(), 2, "t")
            .await
            .expect("page 2");

        // Assert
        assert_eq!(page.mime_type, "image/png");
    }

    #[tokio::test]
    async fn given_page_index_out_of_range_when_requested_then_invalid_input() {
        // Arrange — 1-based indexing: 0 and count+1 are both out of range.
        let repo = FakeRepo::with_file(a_file("/lib/issue.cbz", FileType::Comic));
        let handler = ComicPageHandler::new(FakeAuth, repo, FakeArchive);

        // Act
        let zero = handler.read_page(Uuid::nil(), 0, "t").await;
        let past_end = handler.read_page(Uuid::nil(), 4, "t").await;

        // Assert
        assert!(matches!(zero, Err(DomainError::InvalidInput(_))));
        assert!(matches!(past_end, Err(DomainError::InvalidInput(_))));
    }

    #[tokio::test]
    async fn given_non_comic_file_when_page_requested_then_invalid_input() {
        // Arrange
        let repo = FakeRepo::with_file(a_file("/lib/movie.mp4", FileType::Video));
        let handler = ComicPageHandler::new(FakeAuth, repo, FakeArchive);

        // Act
        let result = handler.read_page(Uuid::nil(), 1, "t").await;

        // Assert
        assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    }

    #[tokio::test]
    async fn given_cbr_comic_when_page_requested_then_invalid_input() {
        // Arrange — RAR has no viable pure-Rust reader, the same precedent
        // `comic_tags.rs` set. The file exists and is genuinely a comic, so
        // this is an unsupported *format*, not a missing record.
        let repo = FakeRepo::with_file(a_file("/lib/issue.cbr", FileType::Comic));
        let handler = ComicPageHandler::new(FakeAuth, repo, FakeArchive);

        // Act
        let result = handler.read_page(Uuid::nil(), 1, "t").await;

        // Assert
        assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    }
}
