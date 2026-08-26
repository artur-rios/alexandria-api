// `SERIAL` is a plain `std::sync::Mutex` held across `.await` deliberately —
// this is a single-threaded-per-test-body serialization guard, not a
// contended lock protecting shared data, so there's no `tokio::sync::Mutex`
// to gain here. Same rationale, same pattern, as `alexandria-ffi/tests/
// parity.rs`'s `SERIAL`.
#![allow(clippy::await_holding_lock)]

//! Integration test for the claim issue #116's design rests on: a catalog
//! listing costs a bounded number of SQL queries, not one per file
//! (Testing Specification §3 — "assert it rather than trusting it").
//!
//! Unit tests against `FakeCatalogRepository` (see `tests/catalog/browse.rs`)
//! prove the handler assembles the right `FileView` shape; they cannot prove
//! the query count, because the fake has no queries to count. Only a real
//! `SqliteCatalogRepository` against real SQLite can pin that.
//!
//! **Why this lives in its own top-level test file, not folded into the
//! `catalog` binary** (`tests/catalog.rs`, alongside `tests/catalog/browse.rs`
//! and friends): `cargo test` compiles each top-level file under `tests/`
//! into its own process, but runs every `#[tokio::test]` *within* one
//! process concurrently by default. The counting mechanism below installs a
//! **process-global** tracing subscriber (see the next paragraph for why it
//! has to be global) that counts every SQL statement sqlx executes anywhere
//! in the process. Sharing a process with `catalog.rs`'s ~300 other tests —
//! most of which also hit SQLite — would make the count include whatever
//! those tests happened to be doing at the same moment: noise, not a proof.
//! Its own process, with its own two tests serialized against each other
//! (`SERIAL` below), sees only the queries this file's own calls issue.
//!
//! **Why the subscriber has to be global, not thread-local**: sqlx's SQLite
//! driver runs actual statement execution on a dedicated worker thread per
//! connection (`sqlx_sqlite::connection::worker`) — SQLite's C API is
//! synchronous, so the async connection is a thin channel to that thread.
//! The `tracing` event that fires when a statement finishes
//! (`sqlx-core`'s `QueryLogger::finish`, target `"sqlx::query"`) therefore
//! fires *on the worker thread*, not on the thread that awaited the query.
//! A thread-local dispatcher installed on the calling thread (the obvious
//! first attempt) never sees it — it silently counts zero. Only a
//! subscriber installed as the process's global default is visible from
//! every thread, worker threads included.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, Once};

use alexandria_core::catalog::model::{FileType, NewFile, StateFilter, SubtypeMetadata};
use alexandria_core::catalog::repos::{CatalogRepository, SqliteCatalogRepository};
use alexandria_core::migrate::run_migrations;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Layer;
use uuid::Uuid;

/// Serializes this file's two tests against each other. Without it, both
/// `#[tokio::test]` functions could run concurrently on different threads
/// of the same process and each would count the other's queries against
/// the shared global counter below.
static SERIAL: Mutex<()> = Mutex::new(());

/// How many `"sqlx::query"` tracing events have fired anywhere in this
/// process since it started. Read via a before/after diff around each
/// measured call — see `count_queries`.
static QUERY_COUNT: AtomicUsize = AtomicUsize::new(0);

/// A `tracing_subscriber::Layer` that increments `QUERY_COUNT` for every
/// event at sqlx's query target, wherever it fires.
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

/// Install `QueryCounter` as the process's global default tracing
/// dispatcher, exactly once. Idempotent so every test can call it freely;
/// `set_global_default` is inherently a once-per-process operation (it
/// errors if called twice), which is exactly what `Once` gives us.
fn init_counter() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let subscriber = tracing_subscriber::registry().with(QueryCounter);
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

/// Run `f` and return `(result, queries_issued)` — the number of
/// `"sqlx::query"` events that fired anywhere in the process while `f` ran.
/// Accurate only because this file's two tests hold `SERIAL` for their
/// entire body, so nothing else in this process is querying SQLite at the
/// same time (see the module doc for why a *global* counter is otherwise
/// unsafe to diff like this).
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

/// Seed `count` audio files, each carrying stored title/artist metadata, so
/// a batch query has something to fetch. Returns nothing — the test only
/// needs the count and the type, not individual uuids.
async fn seed_audio_files(repo: &SqliteCatalogRepository, count: usize) {
    for i in 0..count {
        let uuid = Uuid::new_v4();
        repo.insert_file(NewFile {
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
        repo.update_metadata(
            uuid,
            &SubtypeMetadata::Audio {
                title: Some(format!("Track {i}")),
                artist: Some("Test Artist".into()),
                album: None,
                year: None,
                genre: None,
                track: None,
            },
        )
        .await
        .expect("write metadata");
    }
}

/// The design's headline claim: a listing filtered to one type costs two
/// or three queries (the files query plus one batched subtype query, split
/// across more than one `IN` chunk only once the id count crosses
/// `MAX_SQLITE_PARAMS`) — whatever its size. This pins it at two sizes far
/// apart; if a future change reintroduced a per-row `find_metadata_by_uuid`
/// call, the 200-file run's count would grow in lockstep with the row
/// count instead of staying flat, and this assertion would catch it.
#[tokio::test]
async fn given_single_type_listing_when_listed_then_query_count_is_bounded_not_per_row() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let small_pool = migrated_pool().await;
    let small_repo = SqliteCatalogRepository::new(small_pool);
    seed_audio_files(&small_repo, 5).await;

    let (small_views, small_queries) = count_queries(|| {
        small_repo.list_filtered_view(Some(FileType::Audio), StateFilter::Active, None)
    })
    .await;
    let small_views = small_views.expect("list 5");
    assert_eq!(small_views.len(), 5);

    let large_pool = migrated_pool().await;
    let large_repo = SqliteCatalogRepository::new(large_pool);
    seed_audio_files(&large_repo, 200).await;

    let (large_views, large_queries) = count_queries(|| {
        large_repo.list_filtered_view(Some(FileType::Audio), StateFilter::Active, None)
    })
    .await;
    let large_views = large_views.expect("list 200");
    assert_eq!(large_views.len(), 200);

    assert_eq!(
        small_queries, large_queries,
        "a 40x larger single-type listing must not issue more queries — \
         the query count must not grow with the number of files"
    );
    assert!(
        small_queries <= 3,
        "a single-type listing should cost the files query plus one \
         batched subtype query, got {small_queries}"
    );

    // Every row still carries its own metadata — batching must not have
    // lost anything on the way to being cheap.
    assert!(large_views
        .iter()
        .all(|v| matches!(v.metadata, Some(SubtypeMetadata::Audio { .. }))));
}

/// A mixed-type listing costs one further query per subtype the result
/// actually contains, not one per file and not one for every subtype table
/// that exists regardless of whether the result has any rows of that type.
#[tokio::test]
async fn given_mixed_type_listing_when_listed_then_query_count_scales_with_types_present_not_rows()
{
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let pool = migrated_pool().await;
    let repo = SqliteCatalogRepository::new(pool);
    seed_audio_files(&repo, 50).await;
    // One video file: the result now contains two subtypes.
    let video_uuid = Uuid::new_v4();
    repo.insert_file(NewFile {
        uuid: video_uuid,
        path: "/lib/clip.mp4".into(),
        name: "clip.mp4".into(),
        file_type: FileType::Video,
        content_hash: Some("0".repeat(64)),
        size_bytes: None,
        mtime: None,
        indexed_at: chrono::Utc::now(),
    })
    .await
    .expect("insert video");

    let (views, queries) =
        count_queries(|| repo.list_filtered_view(None, StateFilter::Active, None)).await;
    let views = views.expect("list mixed");
    assert_eq!(views.len(), 51);

    // Files query + audio batch + video batch = 3, regardless of the 50:1
    // skew between the two subtypes.
    assert_eq!(
        queries, 3,
        "a listing spanning two subtypes should cost the files query plus \
         one batch per subtype present, got {queries}"
    );
}
