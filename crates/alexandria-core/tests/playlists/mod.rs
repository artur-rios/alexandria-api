//! Shared fixtures for the playlists integration tests (mirrors
//! `tests/common/mod.rs` at the group level). Tasks 2-6 reuse
//! `create_playlist`, `insert_audio_file`, and `insert_four_audio_files` to
//! seed entries against a real migrated database.

use alexandria_core::catalog::model::{FileType, NewFile};
use alexandria_core::catalog::repos::{CatalogRepository, SqliteCatalogRepository};
use alexandria_core::migrate::migrate_database;
use alexandria_core::playlists::model::{NewPlaylist, Playlist};
use alexandria_core::playlists::repos::{PlaylistRepository, SqlitePlaylistRepository};
use sqlx::sqlite::SqlitePool;
use uuid::Uuid;

/// A real migrated database and its playlist repository (Testing
/// Specification §6.4) — proves migration 17 actually applies, not just
/// that the domain logic is sound against a fake.
pub async fn repo_with_pool() -> (SqlitePlaylistRepository, SqlitePool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("alexandria.sqlite");
    let pool = migrate_database(path.to_str().expect("path"))
        .await
        .expect("migrate");
    (SqlitePlaylistRepository::new(pool.clone()), pool, dir)
}

/// Insert a playlist directly through the repository port, bypassing
/// `CreatePlaylistHandler`'s auth/validation — for tests that need a
/// playlist to already exist and are not exercising creation itself.
/// Unused by Task 1's own test; kept `pub` for Tasks 2-6, which add entries
/// to a playlist that must already exist.
#[allow(dead_code)]
pub async fn create_playlist(repo: &SqlitePlaylistRepository, name: &str) -> Playlist {
    repo.insert_playlist(NewPlaylist {
        uuid: Uuid::new_v4(),
        name: name.to_string(),
    })
    .await
    .expect("insert playlist")
}

/// Insert an audio file directly through the catalog repository, for tests
/// that add entries to a playlist and need a real `file_id` to reference.
/// Unused by Task 1's own test; kept `pub` for Tasks 2-6.
#[allow(dead_code)]
pub async fn insert_audio_file(repo: &SqliteCatalogRepository, path: &str) -> Uuid {
    let uuid = Uuid::new_v4();
    repo.insert_file(NewFile {
        uuid,
        path: path.to_string(),
        name: path.rsplit('/').next().unwrap_or(path).to_string(),
        file_type: FileType::Audio,
        content_hash: Some("0".repeat(64)),
        size_bytes: None,
        mtime: None,
        indexed_at: chrono::Utc::now(),
    })
    .await
    .expect("insert audio file");
    uuid
}

/// Insert four distinct audio files, for tests that exercise ordering
/// across a playlist's entries. Unused by Task 1's own test; kept `pub`
/// for Tasks 3-5 (reordering and reading entries).
#[allow(dead_code)]
pub async fn insert_four_audio_files(repo: &SqliteCatalogRepository) -> [Uuid; 4] {
    [
        insert_audio_file(repo, "/library/track-1.mp3").await,
        insert_audio_file(repo, "/library/track-2.mp3").await,
        insert_audio_file(repo, "/library/track-3.mp3").await,
        insert_audio_file(repo, "/library/track-4.mp3").await,
    ]
}
