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
    use crate::catalog::model::{FileState, FileType};
    use crate::playback::test_support::{a_file, FakeAuth, FakeRepo};

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

    #[tokio::test]
    async fn given_active_video_when_resolved_then_path_mime_and_size_returned() {
        // Arrange — a 2 MB mp4 on disk.
        let repo = FakeRepo::with_file(a_file(
            "/lib/movie.mp4",
            FileType::Video,
            FileState::Active,
            None,
        ));
        let handler = PlaybackSourceHandler::new(
            FakeAuth { good: "t" },
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
        let repo = FakeRepo::with_file(a_file(
            "/lib/gone.mp4",
            FileType::Video,
            FileState::Active,
            None,
        ));
        let handler =
            PlaybackSourceHandler::new(FakeAuth { good: "t" }, repo, FakeStat { size: Err(()) });

        // Act
        let result = handler.resolve(Uuid::nil(), "t").await;

        // Assert
        assert!(matches!(result, Err(DomainError::Disk(_))));
    }

    #[tokio::test]
    async fn given_unknown_extension_when_resolved_then_octet_stream_mime() {
        // Arrange — the catalog indexed it, so playback serves it.
        let repo = FakeRepo::with_file(a_file(
            "/lib/thing.xyz",
            FileType::Text,
            FileState::Active,
            None,
        ));
        let handler =
            PlaybackSourceHandler::new(FakeAuth { good: "t" }, repo, FakeStat { size: Ok(10) });

        // Act
        let source = handler.resolve(Uuid::nil(), "t").await.expect("resolves");

        // Assert
        assert_eq!(source.mime_type, "application/octet-stream");
    }
}
