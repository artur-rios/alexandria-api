//! `DiskThumbnailCache` keys thumbnails as
//! `{uuid}-{mtime_rfc3339_or_"none"}-{THUMBNAIL_MAX_DIM}` (see
//! `playback::thumbnail::ThumbnailHandler::thumbnail`), and an RFC 3339
//! timestamp's time-of-day and UTC-offset fields contain `:` — a character
//! POSIX filesystems accept but Windows refuses in a path component.
//! `DiskThumbnailCache::put` treats a write failure as "log and move on,
//! never fail the request" (its own doc comment), so an unsanitized key
//! would not error there — it would just never populate the cache, on
//! Windows, silently, forever. This pins the round trip against a key shaped
//! exactly like the real one, on a real temp directory (`hashing.rs` is the
//! precedent for touching the filesystem directly in this crate's
//! integration tests), so a regression here shows up as a failing test
//! rather than a quiet no-op cache.

use uuid::Uuid;

use alexandria_core::playback::thumbnail::{DiskThumbnailCache, ThumbnailCache, THUMBNAIL_MAX_DIM};

#[tokio::test]
async fn given_a_key_with_an_rfc3339_mtime_when_cached_on_disk_then_round_trips() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = DiskThumbnailCache::new(dir.path().to_str().expect("utf-8 path").to_string());
    let mtime = chrono::DateTime::from_timestamp(1_700_000_000, 0)
        .expect("valid timestamp")
        .to_rfc3339();
    let key = format!("{}-{mtime}-{THUMBNAIL_MAX_DIM}", Uuid::nil());
    assert!(key.contains(':'), "the fixture must exercise the ':' case");

    // Act
    cache.put(&key, b"JPEG").await.expect("put");
    let read_back = cache.get(&key).await;

    // Assert
    assert_eq!(read_back, Some(b"JPEG".to_vec()));
}
