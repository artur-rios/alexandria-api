//! Media playback (F-10 — UC-38, UC-39, UC-40).
//!
//! Alexandria never modifies or re-encodes the bytes it serves (FR-MP-03).
//! This module resolves a catalog record to on-disk bytes and, for two
//! types, to a bounded derived artifact — a comic page or a thumbnail.

pub mod comic_page;
pub mod mime;
pub mod source;
#[cfg(test)]
pub(crate) mod test_support;
pub mod thumbnail;

use serde::Serialize;
use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::model::{File, FileState};
use crate::catalog::repos::CatalogRepository;
use crate::errors::DomainError;

/// UC-38's FFI payload (FR-MP-06): everything a local player needs to open
/// the file itself. The FFI surface cannot carry a byte stream, so it hands
/// back the resolved path instead and Flutter opens it directly — zero-copy,
/// on the same machine.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackSource {
    pub uuid: Uuid,
    pub path: String,
    pub mime_type: String,
    pub size_bytes: u64,
}

/// Size-of-file port. Split out from `catalog::fs::Filesystem` because
/// playback needs exactly one operation that trait does not have, and unit
/// tests substitute a fake rather than touching a real disk.
#[allow(async_fn_in_trait)]
pub trait FileStat: Send + Sync {
    /// Byte length of the file at `path`. `Err(Disk)` when it cannot be
    /// stat'd — missing, or unreadable.
    async fn size_bytes(&self, path: &str) -> Result<u64, DomainError>;
}

/// Real `FileStat`, backed by `std::fs::metadata` on the blocking pool.
#[derive(Clone, Copy)]
pub struct StdFileStat;

impl FileStat for StdFileStat {
    async fn size_bytes(&self, path: &str) -> Result<u64, DomainError> {
        let owned = path.to_string();
        let handle = tokio::task::spawn_blocking(move || {
            std::fs::metadata(&owned)
                .map(|m| m.len())
                .map_err(|e| DomainError::disk(format!("cannot stat {owned}: {e}")))
        });
        match handle.await {
            Ok(result) => result,
            Err(err) => Err(DomainError::internal(format!("stat task failed: {err}"))),
        }
    }
}

/// The guard every playback use case runs first: authenticate, resolve the
/// UUID, and reject anything that is not playable.
///
/// `missing_at` maps to `Disk`, not `NotFound`. The catalog record exists
/// and is valid — re-index simply found the on-disk file gone (FR-FC-11) —
/// so `NotFound` would tell the caller something false about its own
/// catalog.
pub async fn resolve_playable<A, R>(
    auth: &A,
    repo: &R,
    uuid: Uuid,
    token: &str,
) -> Result<File, DomainError>
where
    A: AuthService,
    R: CatalogRepository,
{
    // The caller must be authenticated before anything else is touched.
    auth.authenticate(token).await?;

    let file = repo
        .find_by_uuid(uuid)
        .await?
        .ok_or(DomainError::NotFound)?;

    if file.state == FileState::Deleted {
        return Err(DomainError::InvalidState);
    }

    if file.missing_at.is_some() {
        return Err(DomainError::disk(format!(
            "file {uuid} is marked missing on disk"
        )));
    }

    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::model::FileType;
    use crate::playback::test_support::{a_file, FakeAuth, FakeRepo};
    use chrono::Utc;

    #[tokio::test]
    async fn given_wrong_token_when_resolved_then_unauthorized() {
        // Arrange
        let auth = FakeAuth { good: "t" };
        let repo = FakeRepo::with_file(a_file(
            "/lib/movie.mp4",
            FileType::Video,
            FileState::Active,
            None,
        ));

        // Act
        let result = resolve_playable(&auth, &repo, Uuid::nil(), "bad").await;

        // Assert
        assert!(matches!(result, Err(DomainError::Unauthorized)));
    }

    #[tokio::test]
    async fn given_unknown_uuid_when_resolved_then_not_found() {
        // Arrange
        let auth = FakeAuth { good: "t" };
        let repo = FakeRepo::none();

        // Act
        let result = resolve_playable(&auth, &repo, Uuid::nil(), "t").await;

        // Assert
        assert!(matches!(result, Err(DomainError::NotFound)));
    }

    #[tokio::test]
    async fn given_soft_deleted_file_when_resolved_then_invalid_state() {
        // Arrange — restore via UC-07 before playing, matching UC-32.
        let auth = FakeAuth { good: "t" };
        let repo = FakeRepo::with_file(a_file(
            "/lib/movie.mp4",
            FileType::Video,
            FileState::Deleted,
            None,
        ));

        // Act
        let result = resolve_playable(&auth, &repo, Uuid::nil(), "t").await;

        // Assert
        assert!(matches!(result, Err(DomainError::InvalidState)));
    }

    #[tokio::test]
    async fn given_missing_at_set_when_resolved_then_disk_error() {
        // Arrange — re-index already found the file gone (FR-FC-11). This is
        // a disk condition, not a NotFound: the catalog record is valid.
        let auth = FakeAuth { good: "t" };
        let repo = FakeRepo::with_file(a_file(
            "/lib/movie.mp4",
            FileType::Video,
            FileState::Active,
            Some(Utc::now()),
        ));

        // Act
        let result = resolve_playable(&auth, &repo, Uuid::nil(), "t").await;

        // Assert
        assert!(matches!(result, Err(DomainError::Disk(_))));
    }

    #[tokio::test]
    async fn given_active_present_file_when_resolved_then_file_returned() {
        // Arrange
        let auth = FakeAuth { good: "t" };
        let repo = FakeRepo::with_file(a_file(
            "/lib/movie.mp4",
            FileType::Video,
            FileState::Active,
            None,
        ));

        // Act
        let result = resolve_playable(&auth, &repo, Uuid::nil(), "t").await;

        // Assert
        assert_eq!(result.expect("resolves").path, "/lib/movie.mp4");
    }
}
