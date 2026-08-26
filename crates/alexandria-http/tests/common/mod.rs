//! Shared helpers for alexandria-http integration tests: a temp on-disk
//! SQLite database wired through `build_services`, and a poll helper that
//! waits for the spawned indexing task to persist rows.

// This module is included by more than one test target (`catalog_api.rs`,
// `collections_api.rs`), and each uses only the helpers its own feature area
// needs — so the rest is dead code as far as the *other* target's compilation
// is concerned. The allow is module-wide rather than per item because the set
// of "unused here" helpers changes with every target that includes the file.
#![allow(dead_code)]

use std::time::Duration;

use alexandria_core::config::{AuthMode, Settings};
use alexandria_core::migrate::migrate_database;
use alexandria_core::services::{self, Services};
use sqlx::sqlite::SqlitePool;
use tempfile::TempDir;
use tower::ServiceExt;

pub struct TestApp {
    pub services: std::sync::Arc<Services>,
    pub pool: SqlitePool,
    /// Kept alive so the underlying SQLite file isn't deleted mid-run.
    _db_dir: TempDir,
}

/// Bearer token every integration test authenticates with. A valid UUID: the
/// active auth mode is local (below), so it must parse as a session id
/// (`LocalAuthService::authenticate`). A matching session is seeded in
/// `test_app()` so it always validates.
pub const TEST_TOKEN: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";

pub async fn test_app() -> TestApp {
    let mut settings = Settings::default();
    settings.auth.mode = AuthMode::Local;
    test_app_with_settings(settings).await
}

/// Like [`test_app`], but lets the caller override `Settings` before
/// `build_services` runs — e.g. to point `playback.thumbnail_cache_dir` at a
/// path inside a test's own `TempDir` instead of the default relative
/// `"thumbnails"`, which would otherwise write into the test process's
/// working directory (the repository itself). `settings.auth.mode` is forced
/// to `Local` regardless of what the caller passed in, matching `test_app`,
/// so `TEST_TOKEN` always authenticates.
pub async fn test_app_with_settings(mut settings: Settings) -> TestApp {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("alexandria.sqlite");
    let pool = migrate_database(db_path.to_str().expect("path"))
        .await
        .expect("migrate");
    seed_session(&pool, TEST_TOKEN).await;

    settings.auth.mode = AuthMode::Local;

    let services = std::sync::Arc::new(services::build_services(&settings, pool.clone()).await);
    TestApp {
        services,
        pool,
        _db_dir: dir,
    }
}

/// Insert a session valid for the next 24h, so `token` authenticates every
/// request an integration test makes under local auth mode.
async fn seed_session(pool: &SqlitePool, token: &str) {
    let now = chrono::Utc::now();
    let expires_at = now + chrono::Duration::hours(24);
    sqlx::query("INSERT INTO sessions (id, created_at, expires_at) VALUES (?, ?, ?)")
        .bind(token)
        .bind(now.to_rfc3339())
        .bind(expires_at.to_rfc3339())
        .execute(pool)
        .await
        .expect("seed session");
}

/// Poll the `files` table until it contains `expected` rows, or time out.
pub async fn wait_for_files(pool: &SqlitePool, expected: i64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files")
            .fetch_one(pool)
            .await
            .expect("count");
        if count.0 >= expected {
            return;
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "timed out waiting for {expected} files; had {} when last checked",
                count.0
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// `(path, name, type, content_hash)` ordered by path.
///
/// `content_hash` is `Option<String>` because the column is nullable and, since
/// indexing stopped hashing whole files (FR-FC-09), usually NULL. Typing it
/// `String` decodes NULL as `""`, which is the exact mistake that shipped as a
/// production parity defect in the FFI accessor (`cf78144`) — a hash that was
/// never computed became indistinguishable from an empty one. A test helper
/// wearing that shape keeps the trap loaded.
pub async fn file_rows(pool: &SqlitePool) -> Vec<(String, String, String, Option<String>)> {
    sqlx::query_as("SELECT path, name, type, content_hash FROM files ORDER BY path")
        .fetch_all(pool)
        .await
        .expect("rows")
}

/// `(uuid, path, name, type, content_hash)` ordered by path. Used by UC-04
/// integration tests to resolve a cataloged file's public UUID for the
/// `PATCH /v1/files/{uuid}/metadata` request. `content_hash` is nullable for
/// the reason [`file_rows`] gives.
pub async fn file_rows_with_uuid(
    pool: &SqlitePool,
) -> Vec<(String, String, String, String, Option<String>)> {
    sqlx::query_as("SELECT uuid, path, name, type, content_hash FROM files ORDER BY path")
        .fetch_all(pool)
        .await
        .expect("rows")
}

/// `(path, name, type, content_hash, missing_at)` — `missing_at` is NULL when
/// the on-disk file was present at last refresh, and `content_hash` is
/// nullable for the reason [`file_rows`] gives.
pub async fn file_rows_with_missing(
    pool: &SqlitePool,
) -> Vec<(String, String, String, Option<String>, Option<String>)> {
    sqlx::query_as(
        "SELECT path, name, type, content_hash, missing_at \
         FROM files ORDER BY path",
    )
    .fetch_all(pool)
    .await
    .expect("rows")
}

/// Poll `GET /v1/index/runs/{runId}` (UC-42) until the run leaves `running`,
/// then return its parsed body.
///
/// `RefreshHandler::refresh_one` processes cataloged paths concurrently, so
/// polling an individual row (or a raw missing-count) says nothing about
/// whether the *other* path's write has landed yet. The run record's own
/// `complete` status is the real signal that the whole walk — every path,
/// not just whichever one happened to finish first — is done, which is what
/// `catalog_api.rs`'s and `run_status_api.rs`'s refresh tests actually need
/// to wait on before reading final row state or a run's tally.
pub async fn wait_for_run_terminal(
    services: &std::sync::Arc<Services>,
    run_id: &str,
    token: &str,
) -> serde_json::Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let request = axum::http::Request::builder()
            .method("GET")
            .uri(format!("/v1/index/runs/{run_id}"))
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::empty())
            .unwrap();
        let response = alexandria_http::app(Settings::default(), services.clone())
            .oneshot(request)
            .await
            .expect("run status oneshot");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        if body["status"] != "running" {
            return body;
        }
        if std::time::Instant::now() > deadline {
            panic!("run {run_id} never left running");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

pub fn write_file(dir: &tempfile::TempDir, name: &str, contents: &[u8]) -> std::path::PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, contents).expect("write");
    path
}

/// A tiny, real, valid JPEG — deterministic per `seed` so a test can
/// recompute the exact bytes an archive entry was written with just by
/// calling this again with the same seed, without `write_cbz` having to hand
/// back a bytes map. The color is derived from `seed`'s bytes, so different
/// entries encode to different (but still real, decodable) JPEGs.
pub fn jpeg_bytes_for(seed: &str) -> Vec<u8> {
    let sum: u32 = seed.bytes().map(u32::from).sum();
    let pixel = image::Rgb([(sum % 256) as u8, ((sum / 3) % 256) as u8, 128]);
    let img = image::RgbImage::from_pixel(4, 4, pixel);
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut out)
        .encode_image(&image::DynamicImage::ImageRgb8(img))
        .expect("encode jpeg");
    out
}

/// Write a real CBZ (ZIP) archive at `dir/name` containing one real JPEG per
/// entry in `entries`, in exactly the order given — callers deliberately
/// pass entries out of page order to prove a reader sorts rather than
/// trusting archive order. Each entry's bytes are `jpeg_bytes_for(entry)`.
pub fn write_cbz(dir: &tempfile::TempDir, name: &str, entries: &[&str]) -> std::path::PathBuf {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let path = dir.path().join(name);
    let file = std::fs::File::create(&path).expect("create cbz file");
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default();

    for entry in entries {
        zip.start_file(*entry, options).expect("start entry");
        zip.write_all(&jpeg_bytes_for(entry)).expect("write entry");
    }

    zip.finish().expect("finish cbz zip");
    path
}

/// Write a real PNG of `width` x `height` at `dir/name`.
pub fn write_image(
    dir: &tempfile::TempDir,
    name: &str,
    width: u32,
    height: u32,
) -> std::path::PathBuf {
    let path = dir.path().join(name);
    let img = image::RgbImage::from_pixel(width, height, image::Rgb([200, 60, 30]));
    image::DynamicImage::ImageRgb8(img)
        .save(&path)
        .expect("write png");
    path
}

/// Write a real, ffmpeg-encoded MP4 of `width` x `height` at `dir/name`.
///
/// Mirrors the `write_minimal_mp4` helper `alexandria-core`'s `video_tags`
/// unit tests and the FFI parity suite already use — MPEG-4 video, ten
/// identical flat frames, no audio. UC-40's video path decodes a real
/// keyframe through ffmpeg, so nothing short of a genuinely encoded file
/// exercises it.
pub fn write_mp4(
    dir: &tempfile::TempDir,
    name: &str,
    width: u32,
    height: u32,
) -> std::path::PathBuf {
    let path = dir.path().join(name);

    ffmpeg_next::init().expect("ffmpeg init");

    let mut octx = ffmpeg_next::format::output(&path).expect("create output context");

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

    for i in 0..10 {
        frame.set_pts(Some(i));
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

    path
}

/// Write a minimal valid single-channel 8-bit PCM WAV file — just enough of
/// a real RIFF/WAVE container for `lofty` to recognize the format and accept
/// a written tag. Mirrors the helper of the same name in `alexandria-core`'s
/// `audio_tags` unit tests; duplicated rather than shared because that one
/// is `#[cfg(test)]`-private to its own crate.
fn write_minimal_wav(path: &std::path::Path) {
    let sample_data: [u8; 8] = [0x80; 8];
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36u32 + sample_data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(b"fmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
    bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
    bytes.extend_from_slice(&8000u32.to_le_bytes()); // sample rate
    bytes.extend_from_slice(&8000u32.to_le_bytes()); // byte rate
    bytes.extend_from_slice(&1u16.to_le_bytes()); // block align
    bytes.extend_from_slice(&8u16.to_le_bytes()); // bits per sample
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(sample_data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&sample_data);
    std::fs::write(path, bytes).expect("write wav");
}

/// Write a real WAV audio file at `dir/name` carrying one embedded
/// front-cover picture (`jpeg` bytes, straight from the caller — real JPEG
/// bytes recommended, but `lofty::picture::Picture::unchecked` does not
/// itself require it) in an ID3v2 tag. The extension the caller passes
/// decides `FileType::Audio` classification the same way it does for any
/// other library file — WAV rather than MP3 because it needs no encoder,
/// only a fixed byte layout, matching `alexandria-core`'s own audio
/// fixtures.
pub fn write_audio_with_cover_art(
    dir: &tempfile::TempDir,
    name: &str,
    jpeg: &[u8],
) -> std::path::PathBuf {
    use lofty::config::WriteOptions;
    use lofty::picture::{MimeType, Picture, PictureType};
    use lofty::tag::{Tag, TagExt, TagType};

    let path = dir.path().join(name);
    write_minimal_wav(&path);

    let mut tag = Tag::new(TagType::Id3v2);
    let picture = Picture::unchecked(jpeg.to_vec())
        .pic_type(PictureType::CoverFront)
        .mime_type(MimeType::Jpeg)
        .build();
    tag.push_picture(picture);
    tag.save_to_path(&path, WriteOptions::default())
        .expect("save cover art tag");

    path
}

/// Write a real WAV audio file at `dir/name` with no embedded tag at all —
/// the "genuinely no cover art" fixture, as distinct from one `lofty` fails
/// to parse.
pub fn write_audio_without_cover_art(dir: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
    let path = dir.path().join(name);
    write_minimal_wav(&path);
    path
}

/// Count entries in a directory, treating a missing directory as zero — the
/// thumbnail cache directory does not exist until the first thumbnail is
/// written.
pub fn count_dir_entries(dir: &std::path::Path) -> usize {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries.count(),
        Err(_) => 0,
    }
}
