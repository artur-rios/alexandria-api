//! UC-38 integration tests for `GET /v1/files/{uuid}/stream` (Testing
//! Specification §7): the real axum router, a real temp SQLite database, and
//! a real on-disk file indexed through `POST /v1/index`. Each test asserts
//! the response, covering the main flow (full read, byte-range reads) plus
//! the alternative flows (unauthenticated, soft-deleted, missing on disk).

mod common;

use alexandria_core::config::Settings;
use alexandria_http::app;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use tempfile::tempdir;
use tower::ServiceExt;

use crate::common::{file_rows_with_uuid, test_app, wait_for_files};

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

fn delete_request(uuid: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(format!("/v1/files/{uuid}"))
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .body(Body::empty())
        .unwrap()
}

fn stream_request(uuid: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(format!("/v1/files/{uuid}/stream"))
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .body(Body::empty())
        .unwrap()
}

fn ranged_stream_request(uuid: &str, range: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(format!("/v1/files/{uuid}/stream"))
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .header("range", range)
        .body(Body::empty())
        .unwrap()
}

fn unauthenticated_stream_request(uuid: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(format!("/v1/files/{uuid}/stream"))
        .body(Body::empty())
        .unwrap()
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_indexed_file_when_streamed_then_bytes_match_disk_exactly() {
    // Arrange — an indexed file with known bytes.
    let lib = tempdir().unwrap();
    let contents = b"hello playback world".to_vec();
    common::write_file(&lib, "sample.txt", &contents);

    let test = test_app().await;
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
        .oneshot(stream_request(&uuid))
        .await
        .expect("one-shot");

    // Assert
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "text/plain"
    );
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.to_vec(), contents);
}

#[tokio::test]
async fn given_cbz_when_streamed_then_catalog_mime_wins_over_servefile_guess() {
    // Arrange — `.cbz` is a case where our catalog MIME table and
    // `ServeFile`'s own `mime_guess`-based fallback genuinely disagree:
    // the catalog table (`alexandria-core/src/playback/mime.rs`) maps it to
    // `application/vnd.comicbook+zip`, while `mime_guess` has no `.cbz`
    // entry and falls back to `application/octet-stream`. The bytes below
    // are not a real zip — UC-38 streams bytes without parsing them, and
    // `.cbz` classifies as Comic purely by extension, so arbitrary bytes
    // index and stream fine.
    let lib = tempdir().unwrap();
    common::write_file(&lib, "issue.cbz", b"not really a zip");

    let test = test_app().await;
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
        .oneshot(stream_request(&uuid))
        .await
        .expect("one-shot");

    // Assert — the catalog table's MIME wins, not `ServeFile`'s guess
    // (which would be `application/octet-stream` for this extension).
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "application/vnd.comicbook+zip"
    );
}

#[tokio::test]
async fn given_range_request_when_streamed_then_partial_content_returned() {
    // Arrange — this is the behavior a video player's seek depends on.
    let lib = tempdir().unwrap();
    common::write_file(&lib, "sample.txt", b"0123456789");

    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    router
        .clone()
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("index");
    wait_for_files(&test.pool, 1).await;
    let rows = file_rows_with_uuid(&test.pool).await;
    let uuid = rows[0].0.clone();

    // Act — bytes 2..=5 inclusive.
    let response = router
        .oneshot(ranged_stream_request(&uuid, "bytes=2-5"))
        .await
        .expect("one-shot");

    // Assert
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response
            .headers()
            .get("content-range")
            .unwrap()
            .to_str()
            .unwrap(),
        "bytes 2-5/10"
    );
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.to_vec(), b"2345".to_vec());
}

#[tokio::test]
async fn given_open_ended_range_when_streamed_then_tail_returned() {
    // Arrange
    let lib = tempdir().unwrap();
    common::write_file(&lib, "sample.txt", b"0123456789");

    let test = test_app().await;
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
        .oneshot(ranged_stream_request(&uuid, "bytes=7-"))
        .await
        .expect("one-shot");

    // Assert
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.to_vec(), b"789".to_vec());
}

#[tokio::test]
async fn given_range_past_end_when_streamed_then_range_not_satisfiable() {
    // Arrange
    let lib = tempdir().unwrap();
    common::write_file(&lib, "sample.txt", b"0123456789");

    let test = test_app().await;
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
        .oneshot(ranged_stream_request(&uuid, "bytes=100-200"))
        .await
        .expect("one-shot");

    // Assert
    assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
}

// ---------------- AF: unauthorized ----------------

#[tokio::test]
async fn given_unauthenticated_caller_when_streamed_then_unauthorized() {
    // Arrange
    let lib = tempdir().unwrap();
    common::write_file(&lib, "sample.txt", b"data");

    let test = test_app().await;
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
        .oneshot(unauthenticated_stream_request(&uuid))
        .await
        .expect("one-shot");

    // Assert
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ---------------- AF: soft-deleted ----------------

#[tokio::test]
async fn given_deleted_file_when_streamed_then_conflict() {
    // Arrange — restore via UC-07 before playing.
    let lib = tempdir().unwrap();
    common::write_file(&lib, "sample.txt", b"data");

    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    router
        .clone()
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("index");
    wait_for_files(&test.pool, 1).await;
    let rows = file_rows_with_uuid(&test.pool).await;
    let uuid = rows[0].0.clone();

    router
        .clone()
        .oneshot(delete_request(&uuid))
        .await
        .expect("soft delete");

    // Act
    let response = router
        .oneshot(stream_request(&uuid))
        .await
        .expect("one-shot");

    // Assert
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

// ---------------- AF: missing on disk ----------------

#[tokio::test]
async fn given_file_deleted_from_disk_when_streamed_then_internal_error() {
    // Arrange — the record is valid; only the bytes are gone. This must not
    // be a 404, which would misreport the catalog.
    let lib = tempdir().unwrap();
    let path = common::write_file(&lib, "sample.txt", b"data");

    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    router
        .clone()
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("index");
    wait_for_files(&test.pool, 1).await;
    let rows = file_rows_with_uuid(&test.pool).await;
    let uuid = rows[0].0.clone();

    std::fs::remove_file(&path).expect("remove fixture from disk");

    // Act
    let response = router
        .oneshot(stream_request(&uuid))
        .await
        .expect("one-shot");

    // Assert
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}
