//! UC-40 integration tests for `GET /v1/files/{uuid}/thumbnail` (Testing
//! Specification §7): the real axum router, a real temp SQLite database, and
//! a real on-disk image (built with the `image` crate) indexed through
//! `POST /v1/index`.
//!
//! `settings.playback.thumbnail_cache_dir` defaults to the relative path
//! `"thumbnails"`, which — left unpointed — would write into the test
//! process's working directory (the repository itself). Every test here
//! routes it into its own `TempDir` via `common::test_app_with_settings`,
//! held alive for the whole test.

mod common;

use alexandria_core::config::Settings;
use alexandria_http::app;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use tempfile::tempdir;
use tower::ServiceExt;

use crate::common::{file_rows_with_uuid, test_app_with_settings, wait_for_files};

fn settings_with_cache_dir(cache_dir: &std::path::Path) -> Settings {
    let mut settings = Settings::default();
    settings.playback.thumbnail_cache_dir = cache_dir.to_str().unwrap().to_string();
    settings
}

fn index_request(root: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .header("content-type", "application/json")
        .body(Body::from(serde_json::json!({ "root": root }).to_string()))
        .unwrap()
}

fn thumbnail_request(uuid: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(format!("/v1/files/{uuid}/thumbnail"))
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn given_image_file_when_thumbnailed_then_jpeg_within_max_dimension() {
    // Arrange — a 1000x500 PNG; the thumbnail must fit 320 and keep 2:1.
    let lib = tempdir().unwrap();
    common::write_image(&lib, "photo.png", 1000, 500);
    let cache_dir = tempdir().unwrap();

    let test = test_app_with_settings(settings_with_cache_dir(cache_dir.path())).await;
    let router = app(Settings::default(), test.services.clone());

    router
        .clone()
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("index");
    wait_for_files(&test.pool, 1).await;
    let rows = file_rows_with_uuid(&test.pool).await;
    let uuid = rows[0].0.clone();

    // Act
    let response = router
        .oneshot(thumbnail_request(&uuid))
        .await
        .expect("one-shot");

    // Assert
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "image/jpeg"
    );
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let decoded = image::load_from_memory(&body).expect("valid jpeg");
    assert_eq!(decoded.width(), 320);
    assert_eq!(decoded.height(), 160);
}

#[tokio::test]
async fn given_thumbnail_requested_twice_then_second_call_is_cached() {
    // Arrange
    let lib = tempdir().unwrap();
    common::write_image(&lib, "photo.png", 400, 400);
    let cache_dir = tempdir().unwrap();

    let test = test_app_with_settings(settings_with_cache_dir(cache_dir.path())).await;
    let router = app(Settings::default(), test.services.clone());

    router
        .clone()
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("index");
    wait_for_files(&test.pool, 1).await;
    let rows = file_rows_with_uuid(&test.pool).await;
    let uuid = rows[0].0.clone();

    // Act
    let first_response = router
        .clone()
        .oneshot(thumbnail_request(&uuid))
        .await
        .expect("one-shot");
    let first = to_bytes(first_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let cached_files_after_first = common::count_dir_entries(cache_dir.path());
    let second_response = router
        .oneshot(thumbnail_request(&uuid))
        .await
        .expect("one-shot");
    let second = to_bytes(second_response.into_body(), usize::MAX)
        .await
        .unwrap();

    // Assert — identical bytes, and the first call populated the cache.
    assert_eq!(first, second);
    assert_eq!(cached_files_after_first, 1);
}

/// Index a single-file library and return the one file's UUID. Every format
/// case below is the same three steps — write, index, wait — differing only
/// in the file written, so they share this.
async fn index_one(
    lib: &tempfile::TempDir,
    cache_dir: &std::path::Path,
) -> (common::TestApp, axum::Router, String) {
    let test = test_app_with_settings(settings_with_cache_dir(cache_dir)).await;
    let router = app(Settings::default(), test.services.clone());

    router
        .clone()
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("index");
    wait_for_files(&test.pool, 1).await;
    let rows = file_rows_with_uuid(&test.pool).await;
    let uuid = rows[0].0.clone();

    (test, router, uuid)
}

/// Every raster extension `classify_by_extension` maps to `FileType::Image`
/// must actually thumbnail — the `image` crate's feature list and that
/// extension table have to agree, or indexing a `.gif` produces a catalog
/// entry whose thumbnail route answers 400 (FR-MP-05).
///
/// One test per format rather than a loop, so a missing decoder names the
/// format it is missing.
async fn assert_format_thumbnails(name: &str) {
    // Arrange — a 1000x500 source; the thumbnail must fit 320 and keep 2:1.
    let lib = tempdir().unwrap();
    common::write_image(&lib, name, 1000, 500);
    let cache_dir = tempdir().unwrap();
    let (_test, router, uuid) = index_one(&lib, cache_dir.path()).await;

    // Act
    let response = router
        .oneshot(thumbnail_request(&uuid))
        .await
        .expect("one-shot");

    // Assert
    assert_eq!(response.status(), StatusCode::OK, "{name} must thumbnail");
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "image/jpeg"
    );
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let decoded = image::load_from_memory(&body).expect("valid jpeg");
    assert_eq!((decoded.width(), decoded.height()), (320, 160));
}

#[tokio::test]
async fn given_gif_image_when_thumbnailed_then_jpeg_within_max_dimension() {
    assert_format_thumbnails("holiday.gif").await;
}

#[tokio::test]
async fn given_bmp_image_when_thumbnailed_then_jpeg_within_max_dimension() {
    assert_format_thumbnails("scan.bmp").await;
}

#[tokio::test]
async fn given_tiff_image_when_thumbnailed_then_jpeg_within_max_dimension() {
    assert_format_thumbnails("scan.tiff").await;
}

#[tokio::test]
async fn given_video_file_when_thumbnailed_then_keyframe_jpeg_within_max_dimension() {
    // Arrange — a real, ffmpeg-encoded 640x360 MP4. This is the only test
    // in the workspace that drives `decode_video_keyframe`: ffmpeg init,
    // best-stream selection, the packet loop, the EOF flush and the
    // stride-aware row copy all run for real here. The design assumed CI
    // can decode video on a read request path; this is what checks it.
    let lib = tempdir().unwrap();
    common::write_mp4(&lib, "clip.mp4", 640, 360);
    let cache_dir = tempdir().unwrap();
    let (_test, router, uuid) = index_one(&lib, cache_dir.path()).await;

    // Act
    let response = router
        .oneshot(thumbnail_request(&uuid))
        .await
        .expect("one-shot");

    // Assert — 16:9 scaled into the 320 box is 320x180.
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "image/jpeg"
    );
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let decoded = image::load_from_memory(&body).expect("valid jpeg");
    assert_eq!((decoded.width(), decoded.height()), (320, 180));
}

#[tokio::test]
async fn given_svg_image_when_thumbnailed_then_bad_request() {
    // Arrange — SVG classifies as `FileType::Image` but is vector, and
    // `image` is a raster crate. Rasterizing would need a new dependency, so
    // the route rejects it explicitly rather than letting the caller see a
    // decode failure.
    let lib = tempdir().unwrap();
    common::write_file(
        &lib,
        "logo.svg",
        br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"></svg>"#,
    );
    let cache_dir = tempdir().unwrap();
    let (_test, router, uuid) = index_one(&lib, cache_dir.path()).await;

    // Act
    let response = router
        .oneshot(thumbnail_request(&uuid))
        .await
        .expect("one-shot");

    // Assert
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn given_document_when_thumbnailed_then_bad_request() {
    // Arrange — FR-MP-05 covers video, image, and comic only.
    let lib = tempdir().unwrap();
    common::write_file(&lib, "book.pdf", b"%PDF-1.4");
    let cache_dir = tempdir().unwrap();

    let test = test_app_with_settings(settings_with_cache_dir(cache_dir.path())).await;
    let router = app(Settings::default(), test.services.clone());

    router
        .clone()
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("index");
    wait_for_files(&test.pool, 1).await;
    let rows = file_rows_with_uuid(&test.pool).await;
    let uuid = rows[0].0.clone();

    // Act
    let response = router
        .oneshot(thumbnail_request(&uuid))
        .await
        .expect("one-shot");

    // Assert
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
