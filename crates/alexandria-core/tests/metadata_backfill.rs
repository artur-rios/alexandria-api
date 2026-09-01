//! Filling the metadata an older extraction never wrote (UC-02), against a
//! real migrated SQLite database.
//!
//! The gap these pin is one a real library hit. `album_artist` arrived in
//! migration 15; extraction had only ever run at first index, so every row
//! written before it holds NULL there — and nothing revisited a row once it
//! was written, because a re-index skips a catalogued path and a refresh
//! compares size and mtime without reading a byte of tags. An artists list
//! grouped by the record's own artist therefore fell back to each track's
//! performer, and a record with guests on it appeared once per guest.
//!
//! The repair has to add without replacing, which is what these tests are
//! mostly about: an owner's own corrections (UC-04) are worth more than
//! anything a tag can say, and a full replace would also blank the fields no
//! file carries — an image's caption, a video's media kind.

use alexandria_core::catalog::model::{FileType, MediaKind, NewFile, SubtypeMetadata};
use alexandria_core::catalog::repos::{CatalogRepository, SqliteCatalogRepository};
use alexandria_core::migrate::run_migrations;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use uuid::Uuid;

async fn migrated_pool() -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");
    run_migrations(&pool).await.expect("migrate");
    pool
}

async fn insert_file(repo: &SqliteCatalogRepository, path: &str, file_type: FileType) -> Uuid {
    let uuid = Uuid::new_v4();
    repo.insert_file(NewFile {
        uuid,
        path: path.to_string(),
        name: path.rsplit('/').next().unwrap_or(path).to_string(),
        file_type,
        content_hash: None,
        size_bytes: None,
        mtime: None,
        indexed_at: chrono::Utc::now(),
    })
    .await
    .expect("insert file");
    uuid
}

fn audio(title: Option<&str>, artist: Option<&str>, album_artist: Option<&str>) -> SubtypeMetadata {
    SubtypeMetadata::Audio {
        title: title.map(str::to_string),
        artist: artist.map(str::to_string),
        album: None,
        year: None,
        genre: None,
        track: None,
        album_artist: album_artist.map(str::to_string),
    }
}

/// The case the owner reported: the tag is in the file and NULL in the
/// catalog, because the row predates the column.
#[tokio::test]
async fn given_a_row_missing_the_album_artist_when_filled_then_it_is_written() {
    let pool = migrated_pool().await;
    let repo = SqliteCatalogRepository::new(pool);
    let uuid = insert_file(&repo, "/library/guest.flac", FileType::Audio).await;
    repo.update_metadata(uuid, &audio(Some("A Guest Spot"), Some("The Guest"), None))
        .await
        .expect("seed the row an older extraction wrote");

    repo.fill_missing_metadata(
        uuid,
        &audio(Some("A Guest Spot"), Some("The Guest"), Some("The Host")),
    )
    .await
    .expect("fill");

    let view = repo.find_metadata_by_uuid(uuid).await.expect("read");
    assert_eq!(
        view,
        Some(audio(
            Some("A Guest Spot"),
            Some("The Guest"),
            Some("The Host")
        ))
    );
}

/// The rule that makes revisiting a row safe at all.
#[tokio::test]
async fn given_a_title_the_owner_corrected_when_filled_then_the_tag_does_not_win() {
    let pool = migrated_pool().await;
    let repo = SqliteCatalogRepository::new(pool);
    let uuid = insert_file(&repo, "/library/misspelt.flac", FileType::Audio).await;
    repo.update_metadata(uuid, &audio(Some("The Owner's Title"), None, None))
        .await
        .expect("seed the owner's own edit (UC-04)");

    repo.fill_missing_metadata(
        uuid,
        &audio(Some("WHAT THE TAG SAYS"), Some("The Guest"), None),
    )
    .await
    .expect("fill");

    let view = repo.find_metadata_by_uuid(uuid).await.expect("read");
    assert_eq!(
        view,
        // The title the owner wrote survives; the artist, which was empty,
        // is filled from the tag.
        Some(audio(Some("The Owner's Title"), Some("The Guest"), None))
    );
}

/// A full replace writes `None` as NULL, which is why the repair cannot be
/// one: no file carries a video's media kind, so a replace would blank the
/// answer the owner gave (FR-ME-02).
#[tokio::test]
async fn given_a_media_kind_the_owner_set_when_filled_then_it_survives() {
    let pool = migrated_pool().await;
    let repo = SqliteCatalogRepository::new(pool);
    let uuid = insert_file(&repo, "/library/film.mkv", FileType::Video).await;
    repo.update_metadata(
        uuid,
        &SubtypeMetadata::Video {
            title: None,
            year: None,
            resolution: None,
            media_kind: Some(MediaKind::Series),
        },
    )
    .await
    .expect("seed the owner's own answer");

    repo.fill_missing_metadata(
        uuid,
        &SubtypeMetadata::Video {
            title: Some("Stalker".to_string()),
            year: Some(1979),
            resolution: None,
            media_kind: None,
        },
    )
    .await
    .expect("fill");

    let view = repo.find_metadata_by_uuid(uuid).await.expect("read");
    assert_eq!(
        view,
        Some(SubtypeMetadata::Video {
            title: Some("Stalker".to_string()),
            year: Some(1979),
            resolution: None,
            media_kind: Some(MediaKind::Series),
        })
    );
}

/// The stamp is what stops the repair from running for the life of the
/// library: a row is re-read once, and then never again.
#[tokio::test]
async fn given_a_stamped_row_when_read_back_then_it_carries_the_version() {
    let pool = migrated_pool().await;
    let repo = SqliteCatalogRepository::new(pool);
    let uuid = insert_file(&repo, "/library/stamped.flac", FileType::Audio).await;

    let before = repo.find_by_uuid(uuid).await.expect("read").expect("file");
    assert_eq!(
        before.metadata_version, 0,
        "a fresh row is behind until extraction has actually run"
    );

    repo.set_metadata_version(uuid, 7).await.expect("stamp");

    let after = repo.find_by_uuid(uuid).await.expect("read").expect("file");
    assert_eq!(after.metadata_version, 7);
}
