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
//! # What each test measures
//!
//! The NFR-02 tests use a plain-text fixture, which measures the pipeline
//! FR-FC-01..09 describes — walk, classify, hash, persist — and nothing else.
//! Extraction is excluded there on purpose: folding ffmpeg's probe speed into
//! a figure labelled "Alexandria's indexing rate" would make the number say
//! less, not more.
//!
//! That cost is instead measured on its own terms by
//! `given_each_media_format_when_indexed_then_extraction_cost_is_measured`,
//! which runs every metadata-carrying subtype (FR-FC-25) through the same
//! indexer against the same text baseline, so the rows are comparable and the
//! only variable is the file the reader is handed. See that test's own note on
//! why its rows are floors rather than forecasts.
//!
//! Knobs (all optional): `ALEXANDRIA_BENCH_FILES` (default 2000),
//! `ALEXANDRIA_BENCH_FILE_BYTES` (default 4096),
//! `ALEXANDRIA_BENCH_MEDIA_FILES` (default 150, quartered for video),
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
use alexandria_core::catalog::index_scope::IndexScope;
use alexandria_core::catalog::queries::browse::{BrowseFilesHandler, FileFilter};
use alexandria_core::catalog::repos::SqliteCatalogRepository;
use alexandria_core::catalog::run_registry::RunRegistry;
use alexandria_core::catalog::runs::SqliteCatalogRunRepository;
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
    SqliteCatalogRunRepository,
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

/// Route the indexer's per-file `warn` to stdout.
///
/// The indexer logs every file it could not index with that file's path and
/// the underlying error, but a test binary installs no `tracing` subscriber,
/// so those events went nowhere. A CI failure could therefore report that two
/// of two thousand files were missing without saying which, or why. With this
/// installed and `--nocapture` (which is how these are run), the warnings land
/// in the same log as the assertion that fires.
///
/// `try_init` rather than `init`: the subscriber is global and these tests
/// share a binary, so the second caller must find it already there rather than
/// panic. `RUST_LOG` still overrides the default if a run wants more detail.
fn install_tracing() {
    use tracing_subscriber::EnvFilter;

    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_test_writer()
        .try_init();
}

/// Render an index run's three outcome counters for an assertion message.
///
/// A bare `assert_eq!(outcome.indexed, files)` says a file is unaccounted for
/// but not which bucket it fell into — skipped (classified out) and failed
/// (read or write error) have completely different causes, and the failure
/// message is all a CI log has to go on.
fn outcome_counters(outcome: &alexandria_core::catalog::commands::index::IndexOutcome) -> String {
    format!(
        "scanned {}, indexed {}, skipped {}, failed {} \
         (a `failed` count is explained by the per-file warnings above)",
        outcome.scanned, outcome.indexed, outcome.skipped, outcome.failed
    )
}

/// Write `count` text files of `bytes` each, spread over subdirectories so the
/// walk has a tree to descend rather than one flat directory — a flat
/// directory of 10k entries has different filesystem characteristics than a
/// real library and would flatter the walk.
fn generate_library(root: &std::path::Path, count: usize, bytes: usize) {
    // Vary the content per file so every hash differs; a library of identical
    // files could let a filesystem or page cache flatter the read path.
    for i in 0..count {
        let dir = fixture_dir(root, i);
        let mut content = format!("file {i}\n").into_bytes();
        content.resize(bytes, b'x');
        std::fs::write(dir.join(format!("f{i:06}.txt")), &content).expect("write fixture file");
    }
}

fn fixture_dir(root: &std::path::Path, i: usize) -> std::path::PathBuf {
    let dir = root.join(format!("d{:03}", i / 100));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    dir
}

/// The subtypes that carry embedded metadata (FR-FC-25), plus `Text` as the
/// no-extraction baseline to measure them against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Text,
    Audio,
    Image,
    Document,
    Comic,
    Video,
}

impl Format {
    /// `Text` first so it prints as the baseline row.
    const ALL: [Format; 6] = [
        Format::Text,
        Format::Audio,
        Format::Image,
        Format::Document,
        Format::Comic,
        Format::Video,
    ];

    fn label(self) -> &'static str {
        match self {
            Format::Text => "text (baseline)",
            Format::Audio => "audio (wav/id3)",
            Format::Image => "image (jpeg/exif)",
            Format::Document => "document (pdf)",
            Format::Comic => "comic (cbz)",
            Format::Video => "video (mp4)",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Format::Text => "txt",
            Format::Audio => "wav",
            Format::Image => "jpg",
            Format::Document => "pdf",
            Format::Comic => "cbz",
            Format::Video => "mp4",
        }
    }

    /// Video is the outlier on both sides: each fixture has to be encoded, and
    /// each probe reads a container. A smaller sample keeps the harness's own
    /// wall-clock sane without changing what a per-second rate means.
    fn count(self, base: usize) -> usize {
        match self {
            Format::Video => (base / 4).max(1),
            _ => base,
        }
    }

    fn write(self, path: &std::path::Path, i: usize) {
        match self {
            Format::Text => {
                std::fs::write(path, format!("file {i}\n")).expect("write txt");
            }
            Format::Audio => {
                write_minimal_wav(path);
                write_audio_tags(path, i);
            }
            Format::Image => {
                write_minimal_jpeg(path);
                write_image_exif(path, i);
            }
            Format::Document => write_minimal_pdf(path, i),
            Format::Comic => write_minimal_cbz(path, i),
            Format::Video => write_minimal_mp4(path, i),
        }
    }
}

/// A minimal valid WAV (8 bytes of PCM) — enough of a container for lofty to
/// probe and for an ID3v2 tag to be written onto.
fn write_minimal_wav(path: &std::path::Path) {
    let sample_data: [u8; 8] = [0x80; 8];
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36u32 + sample_data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(b"fmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
    bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
    bytes.extend_from_slice(&8000u32.to_le_bytes());
    bytes.extend_from_slice(&8000u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&8u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(sample_data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&sample_data);
    std::fs::write(path, &bytes).expect("write wav");
}

fn write_audio_tags(path: &std::path::Path, i: usize) {
    use lofty::config::WriteOptions;
    use lofty::tag::{Accessor, Tag, TagExt, TagType};

    let mut tag = Tag::new(TagType::Id3v2);
    tag.set_title(format!("Title {i}"));
    tag.set_artist(format!("Artist {i}"));
    tag.set_album(format!("Album {i}"));
    tag.set_genre("Bench".to_string());
    // ID3v2 maps no dedicated `Year` item, so this is where lofty's own
    // `set_year` put it before 0.25 removed that method.
    tag.insert_text(lofty::tag::ItemKey::RecordingDate, "2020".to_string());
    tag.set_track(u32::try_from(i % 100).unwrap_or(0));
    tag.save_to_path(path, WriteOptions::default())
        .expect("save id3 tag");
}

fn write_minimal_jpeg(path: &std::path::Path) {
    image::RgbImage::from_pixel(4, 3, image::Rgb([128, 64, 32]))
        .save(path)
        .expect("encode jpeg");
}

fn write_image_exif(path: &std::path::Path, i: usize) {
    use little_exif::exif_tag::ExifTag;
    use little_exif::metadata::Metadata;

    let mut metadata = Metadata::new();
    metadata.set_tag(ExifTag::ImageDescription(format!("Image {i}")));
    metadata.set_tag(ExifTag::ExifImageWidth(vec![4]));
    metadata.set_tag(ExifTag::ExifImageHeight(vec![3]));
    metadata.write_to_file(path).expect("write exif");
}

fn write_minimal_pdf(path: &std::path::Path, i: usize) {
    use lopdf::{dictionary, Document, Object};

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let content = lopdf::content::Content { operations: vec![] };
    let content_id = doc.add_object(lopdf::Stream::new(
        dictionary! {},
        content.encode().expect("encode content"),
    ));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "Contents" => content_id,
        "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1,
        }),
    );
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);
    let info_id = doc.add_object(dictionary! {
        "Title" => Object::string_literal(format!("Doc {i}")),
        "Author" => Object::string_literal("Bench Author"),
    });
    doc.trailer.set("Info", info_id);
    doc.save(path).expect("save pdf");
}

fn write_minimal_cbz(path: &std::path::Path, i: usize) {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let file = std::fs::File::create(path).expect("create cbz");
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default();

    zip.start_file("ComicInfo.xml", options)
        .expect("start ComicInfo.xml");
    let xml = format!(
        r#"<?xml version="1.0"?>
<ComicInfo><Title>Issue {i}</Title><Series>Bench</Series><Number>{i}</Number></ComicInfo>"#
    );
    zip.write_all(xml.as_bytes()).expect("write ComicInfo.xml");

    // 12 pages: a comic's page count is the archive's entry count, so the
    // number of entries is what the reader actually walks.
    for page in 0..12 {
        zip.start_file(format!("page-{page:03}.jpg"), options)
            .expect("start page");
        zip.write_all(b"not-a-real-jpeg-just-bytes")
            .expect("write page");
    }
    zip.finish().expect("finish cbz");
}

fn write_minimal_mp4(path: &std::path::Path, i: usize) {
    ffmpeg_next::init().expect("ffmpeg init");
    let (width, height) = (64u32, 48u32);

    let mut octx = ffmpeg_next::format::output(path).expect("create output context");
    octx.set_metadata({
        let mut dict = ffmpeg_next::Dictionary::new();
        dict.set("title", &format!("Video {i}"));
        dict.set("date", "2024-01-01T00:00:00Z");
        dict
    });

    let codec =
        ffmpeg_next::encoder::find(ffmpeg_next::codec::Id::MPEG4).expect("mpeg4 encoder available");
    let mut ost = octx.add_stream(codec).expect("add video stream");
    let mut encoder = ffmpeg_next::codec::context::Context::new_with_codec(codec)
        .encoder()
        .video()
        .expect("video encoder context");
    encoder.set_width(width);
    encoder.set_height(height);
    encoder.set_format(ffmpeg_next::format::Pixel::YUV420P);
    encoder.set_time_base(ffmpeg_next::Rational(1, 25));
    let mut encoder = encoder.open().expect("open encoder");
    ost.set_parameters(&encoder);

    octx.write_header().expect("write header");

    let mut frame =
        ffmpeg_next::frame::Video::new(ffmpeg_next::format::Pixel::YUV420P, width, height);
    for plane in 0..frame.planes() {
        frame.data_mut(plane).fill(16);
    }
    for pts in 0..10 {
        frame.set_pts(Some(pts));
        encoder.send_frame(&frame).expect("send frame");
        let mut packet = ffmpeg_next::Packet::empty();
        while encoder.receive_packet(&mut packet).is_ok() {
            packet.set_stream(0);
            packet.write_interleaved(&mut octx).expect("write packet");
        }
    }
    encoder.send_eof().expect("send eof");
    let mut packet = ffmpeg_next::Packet::empty();
    while encoder.receive_packet(&mut packet).is_ok() {
        packet.set_stream(0);
        packet.write_interleaved(&mut octx).expect("write packet");
    }
    octx.write_trailer().expect("write trailer");
}

async fn build(
    db: &std::path::Path,
    concurrency: u32,
) -> (BenchIndexHandler, SqliteCatalogRepository) {
    let pool = migrate_database(db.to_str().expect("utf-8 db path"))
        .await
        .expect("migrate");
    let repo = SqliteCatalogRepository::new(pool.clone());
    let run_repo = SqliteCatalogRunRepository::new(pool);
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
        // Low-priority width is not exercised by these throughput
        // benchmarks — every call here builds and runs a `Normal` handler.
        1,
        String::new(),
        run_repo,
        RunRegistry::new(),
    );
    (handler, repo)
}

/// Index `count` fixtures of one format against a fresh database and return
/// `(files_per_second, ms_per_file)`.
///
/// Each call gets its own library tree and its own database, so one format's
/// rows never share a page cache or a `files` table with another's. Extracted
/// as a function because the extraction-cost test calls it once more than it
/// reports — see the warm-up round at that test's top.
async fn measure(format: Format, count: usize, concurrency: u32) -> (f64, f64) {
    let lib = tempfile::tempdir().expect("tempdir");
    let dbdir = tempfile::tempdir().expect("tempdir");

    for i in 0..count {
        let dir = fixture_dir(lib.path(), i);
        format.write(&dir.join(format!("f{i:06}.{}", format.extension())), i);
    }

    let (handler, _repo) = build(&dbdir.path().join("bench.sqlite"), concurrency).await;

    let started = Instant::now();
    let outcome = handler
        .execute(
            lib.path().to_str().expect("utf-8 lib path"),
            Uuid::new_v4(),
            &IndexScope::all(),
        )
        .await
        .expect("index run");
    let elapsed = started.elapsed();

    assert_eq!(
        outcome.indexed,
        count,
        "every {} fixture is cataloged ({})",
        format.label(),
        outcome_counters(&outcome)
    );
    assert_eq!(outcome.failed, 0, "no {} file failed", format.label());

    (
        count as f64 / elapsed.as_secs_f64(),
        elapsed.as_secs_f64() * 1000.0 / count as f64,
    )
}

/// Index a freshly generated library of `count` text files, each `bytes`
/// long, against a fresh database, and return the measured files/sec.
///
/// Fixture generation is excluded from the timed window on purpose — see the
/// size test below, whose whole point is a 1.6 GB fixture tree, for why
/// writing the files must not be allowed to dominate the number this
/// produces.
async fn measure_rate(count: usize, bytes: usize) -> f64 {
    let lib = tempfile::tempdir().expect("tempdir");
    let dbdir = tempfile::tempdir().expect("tempdir");
    generate_library(lib.path(), count, bytes);

    let (handler, _repo) = build(&dbdir.path().join("bench.sqlite"), 4).await;

    let started = Instant::now();
    let outcome = handler
        .execute(
            lib.path().to_str().expect("utf-8 lib path"),
            Uuid::new_v4(),
            &IndexScope::all(),
        )
        .await
        .expect("index run");
    let elapsed = started.elapsed();

    assert_eq!(
        outcome.indexed,
        count,
        "every fixture file is cataloged: {}",
        outcome_counters(&outcome)
    );
    assert_eq!(
        outcome.failed,
        0,
        "no file failed to index: {}",
        outcome_counters(&outcome)
    );

    count as f64 / elapsed.as_secs_f64()
}

/// NFR-02 restated: the indexing rate is independent of the library's size.
///
/// The other cases in this file use small text files, which is why they could
/// not have caught the regression this test exists to prevent — a per-file
/// cost proportional to the file's bytes is invisible to a size-free fixture.
/// A library of 200 × 8 MB files is 1.6 GB; if indexing reads the bytes, this
/// takes seconds to minutes, and if it only stats them, it takes about as
/// long as the same count of empty files.
///
/// The floor is a factor of four, not a tight bound: change detection now
/// keys off `(size_bytes, mtime)` rather than a content hash, so nothing in
/// the scan path should read a byte of file content at all, and the two rates
/// should be nearly identical. But a personal machine's timing noise —
/// scheduler jitter, a page cache that is warm for one tempdir and cold for
/// the other — is real, and this test only needs to catch a regression that
/// shows up as orders of magnitude, not one that shows up as a few percent.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "throughput floor; see the module docs"]
async fn given_large_files_when_indexed_then_the_rate_matches_the_small_file_rate() {
    install_tracing();
    let small = measure_rate(200, 0).await;
    let large = measure_rate(200, 8 * 1024 * 1024).await;

    println!("\nNFR-02 size independence\n  small files: {small:.0} files/sec\n  large files (8 MiB each): {large:.0} files/sec\n");

    assert!(
        large > small / 4.0,
        "indexing rate collapsed on large files ({large:.0} vs {small:.0} files/sec): \
         something is reading file bytes during a scan instead of relying on \
         (size, mtime) change detection"
    );
}

/// NFR-02, first half — the indexing rate.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "measures machine throughput; run explicitly with --ignored --nocapture"]
async fn given_a_generated_library_when_indexed_then_throughput_is_measured() {
    install_tracing();
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
        .execute(
            lib.path().to_str().expect("utf-8 lib path"),
            Uuid::new_v4(),
            &IndexScope::all(),
        )
        .await
        .expect("index run");
    let elapsed = started.elapsed();

    assert_eq!(
        outcome.indexed,
        files,
        "every fixture file is cataloged: {}",
        outcome_counters(&outcome)
    );
    assert_eq!(
        outcome.failed,
        0,
        "no file failed to index: {}",
        outcome_counters(&outcome)
    );

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
    install_tracing();
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
        let outcome = handler
            .execute(&root, Uuid::new_v4(), &IndexScope::all())
            .await;
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
    // Strict on purpose. Contention with the reader is the whole point of this
    // test, so a file lost to it is a real defect, not tolerable noise — the
    // message just has to say which bucket it landed in.
    assert_eq!(
        outcome.indexed,
        files,
        "every fixture file is cataloged despite concurrent reads: {}",
        outcome_counters(&outcome)
    );

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

/// FR-FC-25 — what metadata extraction costs, per format.
///
/// The throughput test above deliberately excludes extraction, which leaves
/// the obvious question unanswered: a real library is mostly media, so how far
/// below the text-file rate does it actually land? This measures each subtype
/// that carries embedded metadata against the same text baseline, through the
/// same indexer, so the rows are directly comparable — the only variable is
/// the file the reader is handed.
///
/// # These are floors, not forecasts
///
/// The fixtures are the *smallest valid file* of each format: an 8-sample WAV,
/// a 4×3 JPEG, a one-page PDF, a 12-entry CBZ, ten frames of 64×48 video. So
/// each row isolates the **fixed per-file cost** — open the container, find
/// the metadata, parse it — with almost no payload to scale over. Real media
/// is orders of magnitude larger, and two costs grow with it: hashing reads
/// every byte, and ffmpeg may seek a long way to find its best video stream.
///
/// Read a row as "extraction costs at least this much per file, before file
/// size enters into it". A library of 4 GB films will not index at the video
/// row's rate.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "measures machine throughput; run explicitly with --ignored --nocapture"]
async fn given_each_media_format_when_indexed_then_extraction_cost_is_measured() {
    let base = env_usize("ALEXANDRIA_BENCH_MEDIA_FILES", 150);
    let concurrency = env_usize("ALEXANDRIA_BENCH_CONCURRENCY", 4) as u32;

    // Warm-up round, discarded. Every row below pays for its own fresh
    // database and fixture tree, but the *first* row also pays the costs that
    // are paid once per process: the first `migrate_database` on a cold page
    // cache, the first touch of the sqlite and ffmpeg code paths, and on
    // Windows the anti-malware scan of a freshly written binary. Those landed
    // on `Text`, which is the baseline every other row is divided by, so the
    // `vs text` column reported audio and document as *faster* than doing no
    // extraction at all — an impossible result, since extraction is strictly
    // extra work on the same walk/hash/persist pipeline. Burning one round
    // first puts the baseline on the same footing as the rows it is compared
    // against. It is `Text` specifically so the warm path is the baseline's
    // own.
    measure(Format::Text, Format::Text.count(base), concurrency).await;

    let mut rows: Vec<(Format, usize, f64, f64)> = Vec::new();

    for format in Format::ALL {
        let count = format.count(base);
        let (per_second, ms_per_file) = measure(format, count, concurrency).await;
        rows.push((format, count, per_second, ms_per_file));
    }

    let baseline = rows
        .iter()
        .find(|(f, ..)| *f == Format::Text)
        .map(|(_, _, rate, _)| *rate)
        .expect("text baseline row");

    println!("\nFR-FC-25 per-format extraction cost (concurrency {concurrency})");
    println!(
        "  {:<20}{:>7}{:>14}{:>11}{:>12}",
        "format", "files", "files/sec", "ms/file", "vs text"
    );
    for (format, count, per_second, ms) in &rows {
        println!(
            "  {:<20}{count:>7}{per_second:>14.0}{ms:>11.2}{:>11.0}%",
            format.label(),
            per_second / baseline * 100.0
        );
    }
    println!(
        "\n  Minimal-size fixtures: these isolate fixed per-file extraction\n\
         \x20 cost. Real media is larger and will index more slowly.\n"
    );

    for (format, _, per_second, _) in &rows {
        assert!(
            *per_second >= 5.0,
            "{} extraction collapsed to {per_second:.1} files/sec",
            format.label()
        );
    }
}
