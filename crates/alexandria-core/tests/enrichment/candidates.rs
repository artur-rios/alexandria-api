//! What `candidates` actually selects, against a real migrated database.
//!
//! This file exists because its absence hid a real defect. Every other
//! enrichment test drives the handler through `FakeEnrichmentRepository`,
//! which answers a canned list and never runs a query — so the one statement
//! the whole feature depends on was never executed against a schema, and it
//! named a table (`files_audio`) that does not exist. A fake repository
//! tests the caller's decisions; only a real one tests the SQL.

use alexandria_core::catalog::model::{FileType, NewFile, SubtypeMetadata};
use alexandria_core::catalog::repos::{CatalogRepository, SqliteCatalogRepository};
use alexandria_core::enrichment::model::{
    ArtistImage, EnrichmentOutcome, EnrichmentScope, TrackLyrics,
};
use alexandria_core::enrichment::repos::{EnrichmentRepository, SqliteEnrichmentRepository};
use alexandria_core::migrate::migrate_database;
use chrono::Utc;
use uuid::Uuid;

async fn fixtures() -> (
    SqliteEnrichmentRepository,
    SqliteCatalogRepository,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("alexandria.sqlite");
    let pool = migrate_database(path.to_str().expect("path"))
        .await
        .expect("migrate");
    (
        SqliteEnrichmentRepository::new(pool.clone()),
        SqliteCatalogRepository::new(pool),
        dir,
    )
}

/// An audio file with tags, which is what a candidate is built from.
async fn insert_tagged(
    catalog: &SqliteCatalogRepository,
    path: &str,
    title: &str,
    artist: &str,
    album_artist: Option<&str>,
) -> Uuid {
    let uuid = Uuid::new_v4();
    catalog
        .insert_file(NewFile {
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
        .expect("insert");

    catalog
        .update_metadata(
            uuid,
            &SubtypeMetadata::Audio {
                title: Some(title.to_string()),
                artist: Some(artist.to_string()),
                album: Some("Kind of Blue".to_string()),
                year: Some(1959),
                genre: None,
                track: Some(1),
                album_artist: album_artist.map(str::to_string),
            },
        )
        .await
        .expect("metadata");

    uuid
}

async fn settle_image(repo: &SqliteEnrichmentRepository, artist: &str) {
    repo.put_artist_image(ArtistImage {
        artist_name: artist.to_string(),
        mbid: Some("mb-1".to_string()),
        source_url: Some("https://commons.example/p.jpg".to_string()),
        image_path: Some("mb-1.jpg".to_string()),
        outcome: EnrichmentOutcome::Found,
        fetched_at: Utc::now(),
    })
    .await
    .expect("image");
}

async fn settle_lyrics(repo: &SqliteEnrichmentRepository, file_uuid: Uuid) {
    repo.put_lyrics(TrackLyrics {
        file_uuid,
        mbid: None,
        plain: Some("a line".to_string()),
        synced: None,
        source: Some("fake".to_string()),
        outcome: EnrichmentOutcome::Found,
        fetched_at: Utc::now(),
    })
    .await
    .expect("lyrics");
}

#[tokio::test]
async fn given_a_tagged_audio_file_when_pending_is_queried_then_its_tags_come_back() {
    // The query the whole feature runs on, against a real schema. Its column
    // and table names are only ever checked here.
    let (repo, catalog, _dir) = fixtures().await;
    insert_tagged(
        &catalog,
        "/library/so-what.flac",
        "So What",
        "Miles Davis",
        Some("Miles Davis"),
    )
    .await;

    let pending = repo
        .candidates(&EnrichmentScope::Pending)
        .await
        .expect("candidates");

    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].title.as_deref(), Some("So What"));
    assert_eq!(pending[0].album_artist.as_deref(), Some("Miles Davis"));
    assert_eq!(pending[0].album.as_deref(), Some("Kind of Blue"));
}

#[tokio::test]
async fn given_the_catalog_when_a_candidate_is_read_then_it_carries_no_duration() {
    // Pinning a real limitation rather than a preference. `audio_files` has
    // no duration column — only `video_files` does — so LRCLIB is queried
    // without the one field that separates a radio edit from an album cut.
    // If a duration column is ever added, this test fails and is the
    // reminder to send it.
    let (repo, catalog, _dir) = fixtures().await;
    insert_tagged(
        &catalog,
        "/library/so-what.flac",
        "So What",
        "Miles Davis",
        None,
    )
    .await;

    let pending = repo
        .candidates(&EnrichmentScope::Pending)
        .await
        .expect("candidates");

    assert_eq!(pending[0].duration_seconds, None);
}

#[tokio::test]
async fn given_both_facts_settled_when_pending_is_queried_then_the_file_is_excluded() {
    // Resumability: a second pass must not re-ask what is already answered.
    // BOTH facts have to be settled -- a file is pending while either one is
    // outstanding.
    let (repo, catalog, _dir) = fixtures().await;
    let uuid = insert_tagged(
        &catalog,
        "/library/so-what.flac",
        "So What",
        "Miles Davis",
        None,
    )
    .await;
    settle_lyrics(&repo, uuid).await;
    settle_image(&repo, "Miles Davis").await;

    let pending = repo
        .candidates(&EnrichmentScope::Pending)
        .await
        .expect("candidates");

    assert!(pending.is_empty());
}

#[tokio::test]
async fn given_settled_lyrics_but_no_image_yet_when_pending_is_queried_then_it_returns() {
    // The other half of the same rule, and the one whose absence stranded
    // artist images: lyrics settled is not the whole story.
    let (repo, catalog, _dir) = fixtures().await;
    let uuid = insert_tagged(
        &catalog,
        "/library/so-what.flac",
        "So What",
        "Miles Davis",
        None,
    )
    .await;
    settle_lyrics(&repo, uuid).await;

    let pending = repo
        .candidates(&EnrichmentScope::Pending)
        .await
        .expect("candidates");

    assert_eq!(pending.len(), 1);
}

#[tokio::test]
async fn given_failed_lyrics_when_pending_is_queried_then_the_file_returns() {
    // The other half: a failure is not an answer, and a later run asks again.
    let (repo, catalog, _dir) = fixtures().await;
    let uuid = insert_tagged(
        &catalog,
        "/library/so-what.flac",
        "So What",
        "Miles Davis",
        None,
    )
    .await;
    repo.put_lyrics(TrackLyrics {
        file_uuid: uuid,
        mbid: None,
        plain: None,
        synced: None,
        source: None,
        outcome: EnrichmentOutcome::Failed,
        fetched_at: Utc::now(),
    })
    .await
    .expect("lyrics");

    let pending = repo
        .candidates(&EnrichmentScope::Pending)
        .await
        .expect("candidates");

    assert_eq!(pending.len(), 1, "a failed lookup was never re-asked");
}

#[tokio::test]
async fn given_settled_lyrics_and_a_failed_image_when_pending_runs_then_the_file_returns() {
    // One run with MusicBrainz down and LRCLIB up settles every file's
    // lyrics and fails every artist image. Selecting on the lyrics outcome
    // alone made those images unreachable by any number of later runs --
    // exactly the resumability the `outcome` column exists to provide,
    // defeated. The file comes back so the image can be retried.
    let (repo, catalog, _dir) = fixtures().await;
    let uuid = insert_tagged(
        &catalog,
        "/library/so-what.flac",
        "So What",
        "Miles Davis",
        Some("Miles Davis"),
    )
    .await;
    settle_lyrics(&repo, uuid).await;
    repo.put_artist_image(ArtistImage {
        artist_name: "Miles Davis".to_string(),
        mbid: None,
        source_url: None,
        image_path: None,
        outcome: EnrichmentOutcome::Failed,
        fetched_at: Utc::now(),
    })
    .await
    .expect("image");

    let pending = repo
        .candidates(&EnrichmentScope::Pending)
        .await
        .expect("candidates");

    assert_eq!(pending.len(), 1, "the failed image was stranded");
}

#[tokio::test]
async fn given_an_unrecognized_stored_outcome_when_pending_is_queried_then_it_is_retried() {
    // The rule has to mean the same thing in Rust and in SQL. Reading it as
    // `outcome <> 'failed'` made an unrecognized value settled here while
    // `EnrichmentOutcome::from_stored` called it retryable -- so a row from a
    // newer version, or a corrupted one, would drop out of every run.
    let (repo, catalog, dir) = fixtures().await;
    let uuid = insert_tagged(
        &catalog,
        "/library/so-what.flac",
        "So What",
        "Miles Davis",
        Some("Miles Davis"),
    )
    .await;
    settle_lyrics(&repo, uuid).await;
    settle_image(&repo, "Miles Davis").await;

    // Written straight to the table: no code path produces this value, which
    // is the point -- a future version's might.
    let pool = migrate_database(dir.path().join("alexandria.sqlite").to_str().expect("path"))
        .await
        .expect("reopen");
    sqlx::query("UPDATE track_lyrics SET outcome = 'partial'")
        .execute(&pool)
        .await
        .expect("update");

    let pending = repo
        .candidates(&EnrichmentScope::Pending)
        .await
        .expect("candidates");

    assert_eq!(
        pending.len(),
        1,
        "an unknown outcome was treated as settled"
    );
}

#[tokio::test]
async fn given_an_artist_scope_when_queried_then_settled_files_are_still_returned() {
    // Naming one artist explicitly is the caller asking for it to be done
    // again, so the settled filter deliberately does not apply.
    let (repo, catalog, _dir) = fixtures().await;
    let uuid = insert_tagged(
        &catalog,
        "/library/so-what.flac",
        "So What",
        "Miles Davis",
        Some("Miles Davis"),
    )
    .await;
    settle_lyrics(&repo, uuid).await;

    let scoped = repo
        .candidates(&EnrichmentScope::Artist("Miles Davis".to_string()))
        .await
        .expect("candidates");

    assert_eq!(scoped.len(), 1);
}

#[tokio::test]
async fn given_a_track_with_no_album_artist_when_the_artist_scope_runs_then_the_performer_matches()
{
    // A library tagged before album artist existed still enriches: the
    // fallback to `artist` is in the query, not only in the handler.
    let (repo, catalog, _dir) = fixtures().await;
    insert_tagged(
        &catalog,
        "/library/blue-train.flac",
        "Blue Train",
        "John Coltrane",
        None,
    )
    .await;

    let scoped = repo
        .candidates(&EnrichmentScope::Artist("John Coltrane".to_string()))
        .await
        .expect("candidates");

    assert_eq!(scoped.len(), 1);
}

#[tokio::test]
async fn given_a_video_file_when_pending_is_queried_then_it_is_not_a_candidate() {
    // Enrichment is audio-only; the JOIN is what enforces it.
    let (repo, catalog, _dir) = fixtures().await;
    catalog
        .insert_file(NewFile {
            uuid: Uuid::new_v4(),
            path: "/library/film.mkv".to_string(),
            name: "film.mkv".to_string(),
            file_type: FileType::Video,
            content_hash: Some("0".repeat(64)),
            size_bytes: None,
            mtime: None,
            indexed_at: Utc::now(),
        })
        .await
        .expect("insert");

    let pending = repo
        .candidates(&EnrichmentScope::Pending)
        .await
        .expect("candidates");

    assert!(pending.is_empty());
}
