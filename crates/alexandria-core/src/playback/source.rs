//! UC-38 — Stream file content (FR-MP-01, FR-MP-06).

use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::repos::CatalogRepository;
use crate::errors::DomainError;
use crate::playback::mime::mime_for_path;
use crate::playback::{resolve_playable, FileStat, PlaybackSource};

/// UC-38 — resolve a catalog UUID to everything needed to play its bytes.
///
/// Generic over auth, catalog repository, and stat so the decision logic is
/// unit-tested against trait fakes, then wired with concrete collaborators
/// at runtime (services.rs). Both surfaces call this handler: HTTP takes the
/// resolved path and streams it through `ServeFile`, FFI serializes the
/// `PlaybackSource` and lets the local client open the file itself.
pub struct PlaybackSourceHandler<A, R, S> {
    auth: A,
    repo: R,
    stat: S,
}

impl<A, R, S> PlaybackSourceHandler<A, R, S>
where
    A: AuthService,
    R: CatalogRepository,
    S: FileStat,
{
    pub fn new(auth: A, repo: R, stat: S) -> Self {
        Self { auth, repo, stat }
    }

    pub async fn resolve(&self, uuid: Uuid, token: &str) -> Result<PlaybackSource, DomainError> {
        let file = resolve_playable(&self.auth, &self.repo, uuid, token).await?;

        // The stat is load-bearing twice: it supplies `size_bytes` for the
        // FFI descriptor, and it is what turns a file that vanished without
        // a re-index into a `Disk` error. Without it, HTTP's `ServeFile`
        // would answer its own 404 and misreport the catalog.
        let size_bytes = self.stat.size_bytes(&file.path).await?;

        Ok(PlaybackSource {
            uuid: file.uuid,
            mime_type: mime_for_path(&file.path).to_string(),
            path: file.path,
            size_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Principal;
    use crate::catalog::model::{File, FileState, FileType, NewFile, StateFilter, SubtypeMetadata};
    use crate::config::AuthMode;
    use chrono::{DateTime, Utc};

    // Reuse the fakes shape from `playback::mod`'s tests; they are private
    // to that module, so define the two this test needs here.

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

    #[derive(Clone)]
    struct FakeStat {
        size: Result<u64, ()>,
    }

    impl FileStat for FakeStat {
        async fn size_bytes(&self, _path: &str) -> Result<u64, DomainError> {
            self.size
                .map_err(|_| DomainError::disk("stat failed".to_string()))
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
    async fn given_active_video_when_resolved_then_path_mime_and_size_returned() {
        // Arrange — a 2 MB mp4 on disk.
        let repo = FakeRepo::with_file(a_file("/lib/movie.mp4", FileType::Video));
        let handler = PlaybackSourceHandler::new(
            FakeAuth,
            repo,
            FakeStat {
                size: Ok(2_097_152),
            },
        );

        // Act
        let source = handler.resolve(Uuid::nil(), "t").await.expect("resolves");

        // Assert
        assert_eq!(source.path, "/lib/movie.mp4");
        assert_eq!(source.mime_type, "video/mp4");
        assert_eq!(source.size_bytes, 2_097_152);
    }

    #[tokio::test]
    async fn given_file_that_cannot_be_stat_when_resolved_then_disk_error() {
        // Arrange — the record is active and not marked missing, but the
        // file vanished since the last re-index. This must be a disk error,
        // not a 404: the catalog record is valid.
        let repo = FakeRepo::with_file(a_file("/lib/gone.mp4", FileType::Video));
        let handler = PlaybackSourceHandler::new(FakeAuth, repo, FakeStat { size: Err(()) });

        // Act
        let result = handler.resolve(Uuid::nil(), "t").await;

        // Assert
        assert!(matches!(result, Err(DomainError::Disk(_))));
    }

    #[tokio::test]
    async fn given_unknown_extension_when_resolved_then_octet_stream_mime() {
        // Arrange — the catalog indexed it, so playback serves it.
        let repo = FakeRepo::with_file(a_file("/lib/thing.xyz", FileType::Text));
        let handler = PlaybackSourceHandler::new(FakeAuth, repo, FakeStat { size: Ok(10) });

        // Act
        let source = handler.resolve(Uuid::nil(), "t").await.expect("resolves");

        // Assert
        assert_eq!(source.mime_type, "application/octet-stream");
    }
}
