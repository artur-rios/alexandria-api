//! Shared fixtures for the play history integration tests.
//!
//! One group-scoped module rather than a helper per file, for the reason
//! `tests/playlists/mod.rs` gives: recording, the rankings, and the purge
//! cascade all need the same "an audio file with these tags exists" setup,
//! and three copies of it would drift.

use alexandria_core::catalog::model::{FileType, NewFile, SubtypeMetadata};
use alexandria_core::catalog::repos::{CatalogRepository, SqliteCatalogRepository};
use alexandria_core::migrate::migrate_database;
use alexandria_core::plays::repos::{PlayRepository, SqlitePlayRepository};
use chrono::{TimeZone, Utc};
use sqlx::sqlite::SqlitePool;
use uuid::Uuid;

/// A real migrated database with both repositories the play tests need
/// (Testing Specification §6.4) — and the first thing that proves
/// migration 24 applies at all.
pub async fn repos_with_pool() -> (
    SqlitePlayRepository,
    SqliteCatalogRepository,
    SqlitePool,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("alexandria.sqlite");
    let pool = migrate_database(path.to_str().expect("path"))
        .await
        .expect("migrate");
    (
        SqlitePlayRepository::new(pool.clone()),
        SqliteCatalogRepository::new(pool.clone()),
        pool,
        dir,
    )
}

/// The tags a seeded track carries. All optional, because what the
/// rankings do with an absent one is most of what these tests are about.
#[derive(Debug, Default, Clone, Copy)]
pub struct Tags<'a> {
    pub title: Option<&'a str>,
    pub artist: Option<&'a str>,
    pub album: Option<&'a str>,
    pub album_artist: Option<&'a str>,
    pub genre: Option<&'a str>,
}

/// Insert an audio file named `name` carrying `tags`, through the catalog's
/// own port rather than raw SQL — the rankings read what indexing writes,
/// so the fixture writes it the way indexing does.
pub async fn insert_track(repo: &SqliteCatalogRepository, name: &str, tags: Tags<'_>) -> Uuid {
    let uuid = Uuid::new_v4();
    repo.insert_file(NewFile {
        uuid,
        path: format!("/library/{name}"),
        name: name.to_string(),
        file_type: FileType::Audio,
        content_hash: Some("0".repeat(64)),
        size_bytes: None,
        mtime: None,
        indexed_at: Utc::now(),
    })
    .await
    .expect("insert audio file");

    repo.update_metadata(
        uuid,
        &SubtypeMetadata::Audio {
            title: tags.title.map(str::to_string),
            artist: tags.artist.map(str::to_string),
            album: tags.album.map(str::to_string),
            year: None,
            genre: tags.genre.map(str::to_string),
            track: None,
            album_artist: tags.album_artist.map(str::to_string),
        },
    )
    .await
    .expect("tag audio file");

    uuid
}

/// Insert a file that is not audio, for the tests that a play of one is
/// refused.
pub async fn insert_text_file(repo: &SqliteCatalogRepository, name: &str) -> Uuid {
    let uuid = Uuid::new_v4();
    repo.insert_file(NewFile {
        uuid,
        path: format!("/library/{name}"),
        name: name.to_string(),
        file_type: FileType::Text,
        content_hash: Some("0".repeat(64)),
        size_bytes: None,
        mtime: None,
        indexed_at: Utc::now(),
    })
    .await
    .expect("insert text file");
    uuid
}

/// Record `times` plays of `file_uuid` through the repository port,
/// bypassing the handler's auth — for the ranking tests, which are about
/// what the counts come out as rather than about who was allowed to write
/// them.
///
/// Each play is stamped a minute after the last so `last_played_at` has
/// something to be the maximum *of*: every play at the same instant would
/// let a "returns any of them" bug pass.
pub async fn record_plays(repo: &SqlitePlayRepository, file_uuid: Uuid, times: i64) {
    for minute in 0..times {
        repo.record(
            file_uuid,
            Utc.with_ymd_and_hms(2026, 9, 3, 12, minute as u32, 0)
                .unwrap(),
        )
        .await
        .expect("record play");
    }
}
