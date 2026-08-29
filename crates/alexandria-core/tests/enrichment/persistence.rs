//! The enrichment tables against a real migrated database (Testing
//! Specification §6.4) — proves migration 18 actually applies, and that the
//! rows survive a round trip, rather than only that the domain logic is
//! sound against a fake.

use alexandria_core::catalog::model::{FileType, NewFile};
use alexandria_core::catalog::repos::{CatalogRepository, SqliteCatalogRepository};
use alexandria_core::enrichment::model::{ArtistImage, EnrichmentOutcome, TrackLyrics};
use alexandria_core::enrichment::repos::{EnrichmentRepository, SqliteEnrichmentRepository};
use alexandria_core::errors::DomainError;
use alexandria_core::migrate::migrate_database;
use chrono::{TimeZone, Utc};
use uuid::Uuid;

async fn repo_with_catalog() -> (
    SqliteEnrichmentRepository,
    SqliteCatalogRepository,
    sqlx::sqlite::SqlitePool,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("alexandria.sqlite");
    let pool = migrate_database(path.to_str().expect("path"))
        .await
        .expect("migrate");
    (
        SqliteEnrichmentRepository::new(pool.clone()),
        SqliteCatalogRepository::new(pool.clone()),
        pool,
        dir,
    )
}

/// How many rows `track_lyrics` holds, counted straight off the table.
///
/// Deliberately not through `EnrichmentRepository::lyrics`: that reads
/// through a JOIN on `files`, so after a purge it answers `None` whether or
/// not the row was actually deleted — an orphan is exactly what it cannot
/// see. Counting the table is the only way to assert the row is gone.
async fn lyrics_row_count(pool: &sqlx::sqlite::SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM track_lyrics")
        .fetch_one(pool)
        .await
        .expect("count")
}

async fn insert_audio(repo: &SqliteCatalogRepository, path: &str) -> Uuid {
    let uuid = Uuid::new_v4();
    repo.insert_file(NewFile {
        uuid,
        path: path.to_string(),
        name: path.rsplit('/').next().unwrap_or(path).to_string(),
        file_type: FileType::Audio,
        content_hash: Some("0".repeat(64)),
        size_bytes: None,
        mtime: None,
        indexed_at: Utc::now(),
    })
    .await
    .expect("insert audio file");
    uuid
}

fn an_image(artist: &str, outcome: EnrichmentOutcome) -> ArtistImage {
    ArtistImage {
        artist_name: artist.to_string(),
        mbid: Some("mb-1".to_string()),
        source_url: Some("https://commons.example/portrait.jpg".to_string()),
        image_path: Some("mb-1.jpg".to_string()),
        outcome,
        fetched_at: Utc.with_ymd_and_hms(2026, 8, 29, 12, 0, 0).unwrap(),
    }
}

#[tokio::test]
async fn given_an_artist_image_when_stored_then_it_reads_back_identically() {
    let (repo, _catalog, _pool, _dir) = repo_with_catalog().await;

    repo.put_artist_image(an_image("Miles Davis", EnrichmentOutcome::Found))
        .await
        .expect("store");

    let read = repo
        .artist_image("Miles Davis")
        .await
        .expect("read")
        .expect("a row");
    assert_eq!(read, an_image("Miles Davis", EnrichmentOutcome::Found));
}

#[tokio::test]
async fn given_an_artist_looked_up_twice_when_stored_then_the_row_is_replaced() {
    // A re-run replaces the previous conclusion rather than accumulating one
    // row per attempt -- which the UNIQUE on artist_name is there to enforce.
    let (repo, _catalog, _pool, _dir) = repo_with_catalog().await;

    repo.put_artist_image(an_image("Miles Davis", EnrichmentOutcome::Failed))
        .await
        .expect("first");
    repo.put_artist_image(an_image("Miles Davis", EnrichmentOutcome::Found))
        .await
        .expect("second");

    let read = repo
        .artist_image("Miles Davis")
        .await
        .expect("read")
        .expect("a row");
    assert_eq!(read.outcome, EnrichmentOutcome::Found);
}

#[tokio::test]
async fn given_lyrics_when_stored_then_they_read_back_for_that_file() {
    let (repo, catalog, _pool, _dir) = repo_with_catalog().await;
    let file_uuid = insert_audio(&catalog, "/library/so-what.flac").await;

    repo.put_lyrics(TrackLyrics {
        file_uuid,
        mbid: None,
        plain: Some("first line\nsecond line".to_string()),
        synced: Some("[00:01.00] first line".to_string()),
        source: Some("fake".to_string()),
        outcome: EnrichmentOutcome::Found,
        fetched_at: Utc.with_ymd_and_hms(2026, 8, 29, 12, 0, 0).unwrap(),
    })
    .await
    .expect("store");

    let read = repo.lyrics(file_uuid).await.expect("read").expect("a row");
    assert_eq!(read.outcome, EnrichmentOutcome::Found);
    assert_eq!(read.synced.as_deref(), Some("[00:01.00] first line"));
    assert_eq!(read.file_uuid, file_uuid);
}

#[tokio::test]
async fn given_a_file_that_does_not_exist_when_lyrics_are_stored_then_it_is_refused() {
    // `track_lyrics` carries no foreign key -- SQLite cannot add one through
    // ALTER TABLE -- so the file id is resolved inside the write's own
    // transaction. Without that check a row would be written pointing at
    // nothing, and nothing else in the schema would catch it.
    let (repo, _catalog, _pool, _dir) = repo_with_catalog().await;

    let outcome = repo
        .put_lyrics(TrackLyrics {
            file_uuid: Uuid::new_v4(),
            mbid: None,
            plain: Some("first line".to_string()),
            synced: None,
            source: Some("fake".to_string()),
            outcome: EnrichmentOutcome::Found,
            fetched_at: Utc::now(),
        })
        .await;

    assert!(matches!(outcome, Err(DomainError::NotFound)));
}

#[tokio::test]
async fn given_no_lookup_yet_when_read_then_there_is_no_row() {
    let (repo, catalog, _pool, _dir) = repo_with_catalog().await;
    let file_uuid = insert_audio(&catalog, "/library/so-what.flac").await;

    assert!(repo.artist_image("Nobody").await.expect("read").is_none());
    assert!(repo.lyrics(file_uuid).await.expect("read").is_none());
}

#[tokio::test]
async fn given_a_purged_file_when_its_lyrics_are_read_then_the_row_is_gone() {
    // `track_lyrics` has no foreign key, so nothing cascades to it. An
    // orphan here is worse than a wasted row: `files.id` is an
    // autoincrement, and a later file reusing that id would inherit another
    // track's words and show them confidently.
    let (repo, catalog, pool, _dir) = repo_with_catalog().await;
    let file_uuid = insert_audio(&catalog, "/library/so-what.flac").await;

    repo.put_lyrics(TrackLyrics {
        file_uuid,
        mbid: None,
        plain: Some("first line".to_string()),
        synced: None,
        source: Some("fake".to_string()),
        outcome: EnrichmentOutcome::Found,
        fetched_at: Utc::now(),
    })
    .await
    .expect("store");
    assert_eq!(
        lyrics_row_count(&pool).await,
        1,
        "nothing was stored to purge"
    );

    catalog.purge(file_uuid).await.expect("purge");

    assert_eq!(
        lyrics_row_count(&pool).await,
        0,
        "the purged file left its lyrics behind, orphaned against a files.id \
         that a later file will reuse"
    );
}

#[tokio::test]
async fn given_a_purged_track_when_the_artist_image_is_read_then_it_survives() {
    // Keyed by artist, not by file: one purged track does not mean the
    // artist has left the library, and deleting their photograph would
    // throw away a lookup every other track by them still uses.
    let (repo, catalog, _pool, _dir) = repo_with_catalog().await;
    let file_uuid = insert_audio(&catalog, "/library/so-what.flac").await;
    repo.put_artist_image(an_image("Miles Davis", EnrichmentOutcome::Found))
        .await
        .expect("store");

    catalog.purge(file_uuid).await.expect("purge");

    assert!(repo
        .artist_image("Miles Davis")
        .await
        .expect("read")
        .is_some());
}
