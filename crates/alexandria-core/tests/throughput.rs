//! NFR-02 verification harness: "index at least 500 files per second on a
//! personal machine without blocking read/query operations".
//!
//! Both halves of that sentence are measured here, against the real
//! collaborators — a real temp-directory library, a real on-disk SQLite
//! database (so WAL and the pool are the ones production uses), the real
//! `StdFilesystem`, and the real metadata readers.
//!
//! # Why these are `#[ignore]`d
//!
//! A throughput floor is a statement about a machine, not about the code. On
//! a shared CI runner the same commit can measure 4× apart between runs, so
//! gating `cargo test --workspace` on 500 files/sec would buy flakiness rather
//! than confidence. They run on request:
//!
//! ```bash
//! cargo test -p alexandria-core --test throughput -- --ignored --nocapture
//! ```
//!
//! `--nocapture` matters: the measured figures are printed, and the figure is
//! the point. The assertions are deliberately loose sanity floors that only a
//! real regression trips (re-serializing the walk, or putting blocking I/O
//! back on the runtime). To assert NFR-02's actual 500/sec target — which the
//! requirement scopes to "a personal machine", i.e. yours, not a runner — set
//! `ALEXANDRIA_NFR_STRICT=1`.
//!
//! # What is and is not measured
//!
//! The library fixture is plain text files. That measures the pipeline
//! FR-FC-01..09 describes: walk, classify, hash, persist. It deliberately
//! excludes per-format metadata extraction (FR-FC-25), whose cost is a
//! property of lofty/lopdf/ffmpeg and the file, not of this crate — a fixture
//! of synthetic MP4s would report ffmpeg's probe speed under the banner of
//! Alexandria's indexing rate. Read the number as "catalog pipeline
//! throughput"; a library of real media will index more slowly, dominated by
//! extraction.
//!
//! Knobs (all optional): `ALEXANDRIA_BENCH_FILES` (default 2000),
//! `ALEXANDRIA_BENCH_FILE_BYTES` (default 4096),
//! `ALEXANDRIA_BENCH_CONCURRENCY` (default 4).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alexandria_core::auth::BearerAuthService;
use alexandria_core::catalog::audio_tags::LoftyAudioMetadataReader;
use alexandria_core::catalog::clock::SystemClock;
use alexandria_core::catalog::comic_tags::CbzComicMetadataReader;
use alexandria_core::catalog::commands::index::IndexHandler;
use alexandria_core::catalog::document_tags::PdfEpubMetadataReader;
use alexandria_core::catalog::fs::StdFilesystem;
use alexandria_core::catalog::image_tags::ExifImageMetadataReader;
use alexandria_core::catalog::queries::browse::{BrowseFilesHandler, FileFilter};
use alexandria_core::catalog::repos::SqliteCatalogRepository;
use alexandria_core::catalog::video_tags::FfmpegVideoMetadataReader;
use alexandria_core::migrate::migrate_database;
use uuid::Uuid;

const TOKEN: &str = "bench-token";

type BenchIndexHandler = IndexHandler<
    BearerAuthService,
    SqliteCatalogRepository,
    StdFilesystem,
    SystemClock,
    LoftyAudioMetadataReader,
    ExifImageMetadataReader,
    PdfEpubMetadataReader,
    FfmpegVideoMetadataReader,
    CbzComicMetadataReader,
>;

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn strict() -> bool {
    std::env::var("ALEXANDRIA_NFR_STRICT").is_ok_and(|v| v != "0" && !v.is_empty())
}

/// Write `count` text files of `bytes` each, spread over subdirectories so the
/// walk has a tree to descend rather than one flat directory — a flat
/// directory of 10k entries has different filesystem characteristics than a
/// real library and would flatter the walk.
fn generate_library(root: &std::path::Path, count: usize, bytes: usize) {
    // Vary the content per file so every hash differs; a library of identical
    // files could let a filesystem or page cache flatter the read path.
    for i in 0..count {
        let dir = root.join(format!("d{:03}", i / 100));
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        let mut content = format!("file {i}\n").into_bytes();
        content.resize(bytes, b'x');
        std::fs::write(dir.join(format!("f{i:06}.txt")), &content).expect("write fixture file");
    }
}

async fn build(
    db: &std::path::Path,
    concurrency: u32,
) -> (BenchIndexHandler, SqliteCatalogRepository) {
    let pool = migrate_database(db.to_str().expect("utf-8 db path"))
        .await
        .expect("migrate");
    let repo = SqliteCatalogRepository::new(pool);
    let handler = IndexHandler::new(
        BearerAuthService,
        repo.clone(),
        StdFilesystem,
        SystemClock,
        LoftyAudioMetadataReader,
        ExifImageMetadataReader,
        PdfEpubMetadataReader,
        FfmpegVideoMetadataReader,
        CbzComicMetadataReader,
        concurrency,
    );
    (handler, repo)
}

/// NFR-02, first half — the indexing rate.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "measures machine throughput; run explicitly with --ignored --nocapture"]
async fn given_a_generated_library_when_indexed_then_throughput_is_measured() {
    let files = env_usize("ALEXANDRIA_BENCH_FILES", 2000);
    let bytes = env_usize("ALEXANDRIA_BENCH_FILE_BYTES", 4096);
    let concurrency = env_usize("ALEXANDRIA_BENCH_CONCURRENCY", 4) as u32;

    let lib = tempfile::tempdir().expect("tempdir");
    let dbdir = tempfile::tempdir().expect("tempdir");
    generate_library(lib.path(), files, bytes);

    let (handler, _repo) = build(&dbdir.path().join("bench.sqlite"), concurrency).await;

    // Fixture generation is outside the measurement; only the run is timed.
    let started = Instant::now();
    let outcome = handler
        .execute(lib.path().to_str().expect("utf-8 lib path"), Uuid::new_v4())
        .await
        .expect("index run");
    let elapsed = started.elapsed();

    assert_eq!(outcome.indexed, files, "every fixture file is cataloged");
    assert_eq!(outcome.failed, 0, "no file failed to index");

    let per_second = files as f64 / elapsed.as_secs_f64();
    println!(
        "\nNFR-02 indexing throughput\n\
         \x20 files:       {files}\n\
         \x20 file size:   {bytes} B\n\
         \x20 concurrency: {concurrency}\n\
         \x20 elapsed:     {:.2} s\n\
         \x20 rate:        {per_second:.0} files/sec  (NFR-02 target: 500)\n",
        elapsed.as_secs_f64()
    );

    if strict() {
        assert!(
            per_second >= 500.0,
            "NFR-02 requires >= 500 files/sec, measured {per_second:.0}"
        );
    } else {
        // A floor low enough that only a real regression trips it — the
        // sequential walk this replaced managed far more than this.
        assert!(
            per_second >= 50.0,
            "indexing collapsed to {per_second:.0} files/sec; \
             something has re-serialized the walk or blocked the runtime"
        );
    }
}

/// NFR-02, second half — "without blocking read/query operations".
///
/// This is the half worth having in the suite permanently: it is the property
/// that silently regresses the moment someone calls blocking I/O straight from
/// an `async fn` again. While a run is in flight, reads are issued in a tight
/// loop and their latencies recorded; a runtime whose workers are parked by
/// `std::fs` shows up immediately as reads that take hundreds of milliseconds
/// or stop completing at all.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "measures machine latency; run explicitly with --ignored --nocapture"]
async fn given_an_index_run_in_flight_when_reads_are_issued_then_they_are_not_blocked() {
    let files = env_usize("ALEXANDRIA_BENCH_FILES", 2000);
    let bytes = env_usize("ALEXANDRIA_BENCH_FILE_BYTES", 4096);
    let concurrency = env_usize("ALEXANDRIA_BENCH_CONCURRENCY", 4) as u32;

    let lib = tempfile::tempdir().expect("tempdir");
    let dbdir = tempfile::tempdir().expect("tempdir");
    generate_library(lib.path(), files, bytes);

    let (handler, repo) = build(&dbdir.path().join("bench.sqlite"), concurrency).await;
    let handler = Arc::new(handler);
    let browse = BrowseFilesHandler::new(BearerAuthService, repo);

    let root = lib.path().to_str().expect("utf-8 lib path").to_string();
    let indexing = Arc::new(AtomicBool::new(true));

    let done = indexing.clone();
    let index_task = tokio::spawn(async move {
        let outcome = handler.execute(&root, Uuid::new_v4()).await;
        done.store(false, Ordering::SeqCst);
        outcome
    });

    let mut latencies: Vec<Duration> = Vec::new();
    while indexing.load(Ordering::SeqCst) {
        let started = Instant::now();
        browse
            .list(FileFilter::new(), TOKEN)
            .await
            .expect("read during indexing");
        latencies.push(started.elapsed());
        // Yield rather than spin flat out, so the reader is a realistic client
        // rather than a second load generator competing for the pool.
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    let outcome = index_task.await.expect("index task").expect("index run");
    assert_eq!(outcome.indexed, files);

    assert!(
        latencies.len() >= 10,
        "only {} reads completed during the run — reads are being starved",
        latencies.len()
    );

    latencies.sort_unstable();
    let p95 = latencies[(latencies.len() * 95 / 100).min(latencies.len() - 1)];
    let worst = *latencies.last().expect("non-empty");
    println!(
        "\nNFR-02 read latency during indexing\n\
         \x20 reads:  {}\n\
         \x20 p95:    {:.1} ms  (NFR-01 target: < 200)\n\
         \x20 worst:  {:.1} ms\n",
        latencies.len(),
        p95.as_secs_f64() * 1000.0,
        worst.as_secs_f64() * 1000.0
    );

    if strict() {
        assert!(
            p95 < Duration::from_millis(200),
            "NFR-01 requires p95 < 200 ms; measured {:.1} ms while indexing",
            p95.as_secs_f64() * 1000.0
        );
    } else {
        // Loose enough to survive a noisy runner, tight enough that a runtime
        // worker parked on `std::fs` for the length of a hash cannot hide.
        assert!(
            p95 < Duration::from_secs(2),
            "reads stalled to {:.1} ms p95 during indexing — blocking I/O is \
             likely back on the async runtime",
            p95.as_secs_f64() * 1000.0
        );
    }
}
