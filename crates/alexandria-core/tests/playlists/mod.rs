//! Shared fixtures for the playlists integration tests.
//!
//! Deliberate divergence from how the sibling test groups are laid out:
//! `tests/reading_lists/`, `tests/watchlists/`, `tests/catalog/`, and
//! `tests/repos_integrity.rs` each define their own private, per-file
//! helper instead of sharing one (`catalog/runs.rs` has its own
//! `repo_with_pool`, `repos_integrity.rs` its own
//! `migrated_pool`/`insert_file`). Here that would mean copying
//! `repo_with_pool`, `create_playlist`, `insert_audio_file`, and
//! `insert_four_audio_files` into as many as six files — Tasks 1-6 all seed
//! playlists and audio files against a real migrated database — where six
//! near-identical copies would drift apart over the life of the feature.
//! One group-scoped module, playing `tests/common/mod.rs`'s role but
//! scoped to this one group instead of the whole workspace, is worth the
//! departure from the sibling groups' convention.

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
/// Used by Task 2 onward, which need a playlist to already exist.
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

/// Insert a non-audio (text) file directly through the catalog repository,
/// for the Task 3 test that `add_entries` rejects a non-audio file (a
/// playlist holds audio only -- video and documents have their own
/// watchlists/reading lists).
pub async fn insert_text_file(repo: &SqliteCatalogRepository, path: &str) -> Uuid {
    let uuid = Uuid::new_v4();
    repo.insert_file(NewFile {
        uuid,
        path: path.to_string(),
        name: path.rsplit('/').next().unwrap_or(path).to_string(),
        file_type: FileType::Text,
        content_hash: Some("0".repeat(64)),
        size_bytes: None,
        mtime: None,
        indexed_at: chrono::Utc::now(),
    })
    .await
    .expect("insert text file");
    uuid
}
