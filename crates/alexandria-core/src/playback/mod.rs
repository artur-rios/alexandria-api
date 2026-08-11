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

/// Ceiling on any single blob playback pulls into memory before it can decode
/// it — a source image for a thumbnail, or one decompressed entry of a comic
/// archive.
///
/// Both of those reads sit on a *request* path, not on owner-initiated
/// indexing, and both are sized by data an attacker who can put a file in the
/// library controls: a 3 GB TIFF allocates 3 GB before `image` ever applies
/// its own decode guard, and a 900 KB CBZ whose entry inflates to 40 GB is
/// the classic zip bomb — a single request allocates until the process dies.
///
/// 256 MiB sits an order of magnitude above anything a real library holds and
/// well below anything that threatens the process. A 100-megapixel 8-bit RGB
/// photo is roughly 25 MB compressed and a comic page is a JPEG of a few MB;
/// on the other side `image` already refuses to allocate more than 512 MB of
/// *decoded* pixels, so a source past this cap could almost never have
/// produced a thumbnail anyway. Deliberately a constant and not a config key:
/// it is a safety limit rather than a preference, and a deployment able to
/// raise it would be equally able to disable it.
pub const MAX_PLAYBACK_READ_BYTES: u64 = 256 * 1024 * 1024;

/// Read at most `cap` bytes from `path`, refusing anything larger.
///
/// `tokio::fs::read` allocates the whole file first, which is exactly the
/// behavior being bounded here. Reading `cap + 1` bytes costs one byte over
/// the limit and never truncates — a silently short JPEG decodes into a
/// garbage thumbnail, which is worse than an error.
///
/// Over-cap is `InvalidInput`, not `Disk`: nothing failed to read. The file is
/// simply not something this route can work with, the same classification the
/// SVG and CBR rejections already use, and the same `400` the error table
/// promises for an unsupported source. The message names no path: an
/// `InvalidInput` message is rendered into the client's error envelope.
///
/// `cap` is a parameter rather than a read of [`MAX_PLAYBACK_READ_BYTES`], so
/// a test can drive the over-cap branch against a fixture of a few bytes.
pub(crate) async fn read_capped(path: &str, cap: u64) -> Result<Vec<u8>, DomainError> {
    use tokio::io::AsyncReadExt;

    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| DomainError::disk(format!("cannot read {path}: {e}")))?;

    let mut bytes = Vec::new();
    file.take(cap + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| DomainError::disk(format!("cannot read {path}: {e}")))?;

    if bytes.len() as u64 > cap {
        return Err(DomainError::InvalidInput(format!(
            "file is larger than the {cap}-byte playback read limit"
        )));
    }

    Ok(bytes)
}

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
    async fn given_source_larger_than_cap_when_read_then_invalid_input() {
        // Arrange — 64 bytes on disk against a 16-byte cap. `cap` is a
        // parameter precisely so this fixture stays tiny; the request path
        // passes `MAX_PLAYBACK_READ_BYTES`. Reaching this branch at all
        // proves the read stopped short of allocating the whole file.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("huge.tiff");
        std::fs::write(&path, vec![7u8; 64]).expect("write fixture");

        // Act
        let result = read_capped(path.to_str().expect("path"), 16).await;

        // Assert — an error, not a truncated 16-byte buffer that would
        // decode into a garbage thumbnail.
        assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    }

    #[tokio::test]
    async fn given_source_exactly_at_cap_when_read_then_all_bytes_returned() {
        // Arrange — a file the same size as the cap is legal, and the extra
        // byte the reader is allowed must not turn it into a rejection.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("exact.png");
        std::fs::write(&path, vec![7u8; 16]).expect("write fixture");

        // Act
        let bytes = read_capped(path.to_str().expect("path"), 16).await;

        // Assert
        assert_eq!(bytes.expect("read"), vec![7u8; 16]);
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
