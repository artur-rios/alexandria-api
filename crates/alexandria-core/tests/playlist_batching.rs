// `SERIAL` is a plain `std::sync::Mutex` held across `.await` deliberately —
// this is a single-threaded-per-test-body serialization guard, not a
// contended lock protecting shared data, so there's no `tokio::sync::Mutex`
// to gain here. Same rationale, same pattern, as `browse_batching.rs`'s
// `SERIAL`.
#![allow(clippy::await_holding_lock)]

//! Integration test for Task 6's query-count claim: reading a playlist back
//! costs a bounded number of SQL queries, not one per track (design section
//! 5 / the same "assert it rather than trusting it" rationale as
//! `browse_batching.rs`, which this file is modelled on almost verbatim —
//! see that file's module doc for the full explanation of why the counting
//! mechanism needs its own process and a process-global tracing
//! subscriber).
//!
//! Unit tests against a fake `PlaylistRepository` (none needed here — every
//! playlist test in `tests/playlists/browse.rs` runs against a real
//! `SqlitePlaylistRepository`) cannot prove the query count, because a fake
//! has no queries to count. Only a real repository against real SQLite can
//! pin that — a per-track query would return exactly the right rows, only
//! slowly, and every other test would still pass.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, Once};

use alexandria_core::catalog::model::{FileType, NewFile, SubtypeMetadata};
use alexandria_core::catalog::repos::{CatalogRepository, SqliteCatalogRepository};
use alexandria_core::migrate::run_migrations;
use alexandria_core::playlists::model::NewPlaylist;
use alexandria_core::playlists::repos::{PlaylistRepository, SqlitePlaylistRepository};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Layer;
use uuid::Uuid;

/// Serializes this file's tests against each other -- see
/// `browse_batching.rs`'s `SERIAL` for why a *global* query counter is
/// otherwise unsafe to diff across concurrently running tests.
static SERIAL: Mutex<()> = Mutex::new(());

/// How many `"sqlx::query"` tracing events have fired anywhere in this
/// process since it started. Read via a before/after diff -- see
/// `count_queries`.
static QUERY_COUNT: AtomicUsize = AtomicUsize::new(0);

struct QueryCounter;

impl<S: tracing::Subscriber> Layer<S> for QueryCounter {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if event.metadata().target() == "sqlx::query" {
            QUERY_COUNT.fetch_add(1, Ordering::SeqCst);
        }
    }
}

fn init_counter() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let subscriber = tracing_subscriber::registry().with(QueryCounter);
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

/// Run `f` and return `(result, queries_issued)` -- see `browse_batching.
/// rs`'s identical helper for why this is only accurate while `SERIAL` is
/// held for the whole test body.
async fn count_queries<F, Fut, T>(f: F) -> (T, usize)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    init_counter();
    let before = QUERY_COUNT.load(Ordering::SeqCst);
    let result = f().await;
    let after = QUERY_COUNT.load(Ordering::SeqCst);
    (result, after - before)
}

async fn migrated_pool() -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");
    run_migrations(&pool).await.expect("migrate");
    pool
}

/// Build a playlist of `count` tracks, each a distinct audio file carrying
/// stored title/artist metadata (so the batched audio query has something
/// to fetch), then read it back through `PlaylistRepository::list_view` --
/// the method `BrowsePlaylistsHandler::read` calls, and the one the design
/// section 5 claim is actually about -- and return how many SQL queries
/// that one read issued. Measured at the repository, the same way
/// `browse_batching.rs` measures `list_filtered_view` directly rather than
/// through a handler: the auth check ahead of it is orthogonal to the
/// query-count claim and would only add a constant that cancels out of the
/// `small == large` comparison anyway.
async fn query_count_reading_playlist_of(count: usize) -> usize {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let pool = migrated_pool().await;
    let catalog_repo = SqliteCatalogRepository::new(pool.clone());
    let playlist_repo = SqlitePlaylistRepository::new(pool.clone());

    let playlist = playlist_repo
        .insert_playlist(NewPlaylist {
            uuid: Uuid::new_v4(),
            name: "Batching".into(),
        })
        .await
        .expect("insert playlist");

    let mut file_uuids = Vec::with_capacity(count);
    for i in 0..count {
        let uuid = Uuid::new_v4();
        catalog_repo
            .insert_file(NewFile {
                uuid,
                path: format!("/lib/track-{i:04}.mp3"),
                name: format!("track-{i:04}.mp3"),
                file_type: FileType::Audio,
                content_hash: Some("0".repeat(64)),
                size_bytes: None,
                mtime: None,
                indexed_at: chrono::Utc::now(),
            })
            .await
            .expect("insert file");
        catalog_repo
            .update_metadata(
                uuid,
                &SubtypeMetadata::Audio {
                    title: Some(format!("Track {i}")),
                    artist: Some("Test Artist".into()),
                    album: None,
                    year: None,
                    genre: None,
                    track: None,
                    album_artist: None,
                },
            )
            .await
            .expect("write metadata");
        file_uuids.push(uuid);
    }
    playlist_repo
        .add_entries(playlist.uuid, &file_uuids)
        .await
        .expect("add entries");

    let (entries, queries) = count_queries(|| playlist_repo.list_view(playlist.uuid)).await;
    let entries = entries.expect("list view");
    assert_eq!(entries.len(), count);

    queries
}

/// The claim design section 5 rests on: a playlist read costs a bounded
/// number of SQL queries, not one per track -- a 40x larger playlist must
/// not issue more queries than a small one.
///
/// Asserting only `small == large` fails *open*: `browse_batching.rs`'s
/// module doc explains why the counting mechanism can silently read zero
/// (the `"sqlx::query"` target is sqlx's own private implementation detail,
/// and `init_counter` swallows `set_global_default`'s error if some other
/// subscriber wins the race) -- and `0 == 0` would pass this assertion just
/// as happily as `2 == 2` does. Pinning the actual number, the same way
/// `browse_batching.rs` does, is what turns a broken counter into a loud
/// failure instead of a silent pass.
#[tokio::test]
async fn given_a_large_playlist_when_read_then_the_query_count_does_not_grow_with_it() {
    let small = query_count_reading_playlist_of(5).await;
    let large = query_count_reading_playlist_of(200).await;

    assert_eq!(
        small, large,
        "reading a playlist issues a query per track: {small} for 5, {large} for 200"
    );
    assert_eq!(
        small, 2,
        "reading a playlist under the chunk boundary should cost exactly the \
         entries+files query plus one batched audio query"
    );
    assert_eq!(large, 2);
}

/// `MAX_SQLITE_PARAMS` (`playlists::repos`) is 900, mirroring the catalog's
/// own conservative assumption about SQLite's compiled-in bound-parameter
/// ceiling (see that constant's doc comment). Nothing else in this file
/// proves the chunking actually splits: `given_a_large_playlist_when_read_
/// then_the_query_count_does_not_grow_with_it`'s two sizes (5 and 200) are
/// both far under 900, so `ids.chunks(..)` never runs more than once in
/// either -- deleting the chunking loop entirely would still pass that
/// test, and a playlist past the real boundary would then fail at runtime
/// with "too many SQL variables" at a size nothing here covers.
///
/// This seeds 901 distinct audio files -- one past the boundary -- so the
/// audio batch is forced to split into two `IN` chunks (900 + 1), mirroring
/// `browse_batching.rs`'s equivalent test. Asserts three things at once:
/// every one of the 901 tracks is still present (no id lost at the chunk
/// seam), every track still carries its metadata (the second, one-id chunk
/// is not silently dropped), and the query count is exactly 3 (the
/// entries+files query, plus two audio chunks) -- proving the chunk
/// arithmetic, not just asserting a size passed.
#[tokio::test]
async fn given_a_playlist_past_the_chunk_boundary_when_read_then_every_track_is_still_covered() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let pool = migrated_pool().await;
    let catalog_repo = SqliteCatalogRepository::new(pool.clone());
    let playlist_repo = SqlitePlaylistRepository::new(pool.clone());

    let playlist = playlist_repo
        .insert_playlist(NewPlaylist {
            uuid: Uuid::new_v4(),
            name: "Past the boundary".into(),
        })
        .await
        .expect("insert playlist");

    let count = 901;
    let mut file_uuids = Vec::with_capacity(count);
    for i in 0..count {
        let uuid = Uuid::new_v4();
        catalog_repo
            .insert_file(NewFile {
                uuid,
                path: format!("/lib/track-{i:04}.mp3"),
                name: format!("track-{i:04}.mp3"),
                file_type: FileType::Audio,
                content_hash: Some("0".repeat(64)),
                size_bytes: None,
                mtime: None,
                indexed_at: chrono::Utc::now(),
            })
            .await
            .expect("insert file");
        catalog_repo
            .update_metadata(
                uuid,
                &SubtypeMetadata::Audio {
                    title: Some(format!("Track {i}")),
                    artist: Some("Test Artist".into()),
                    album: None,
                    year: None,
                    genre: None,
                    track: None,
                    album_artist: None,
                },
            )
            .await
            .expect("write metadata");
        file_uuids.push(uuid);
    }
    playlist_repo
        .add_entries(playlist.uuid, &file_uuids)
        .await
        .expect("add entries");

    let (entries, queries) = count_queries(|| playlist_repo.list_view(playlist.uuid)).await;
    let entries = entries.expect("list view");

    assert_eq!(
        entries.len(),
        count,
        "every track must survive the chunk split"
    );
    assert!(
        entries
            .iter()
            .all(|t| matches!(t.file.metadata, Some(SubtypeMetadata::Audio { .. }))),
        "every track, including those in the second one-id chunk, must still \
         carry its metadata"
    );
    assert_eq!(
        queries, 3,
        "901 distinct tracks at a 900-id chunk size should cost the \
         entries+files query plus two audio chunks (900 + 1), got {queries}"
    );
}
