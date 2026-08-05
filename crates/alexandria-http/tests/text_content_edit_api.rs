//! UC-33 integration tests for `PUT /v1/files/{uuid}/content` (Testing
//! Specification §7): the real axum router, a real temp SQLite database,
//! and a real on-disk file indexed through `POST /v1/index`. Each test
//! asserts the response and, where relevant, the resulting on-disk and
//! catalog state, and covers the main flow plus every alternative flow
//! (AF-01 wrong file type, AF-04 not found, AF-05 unauthorized).

mod common;

use alexandria_core::config::Settings;
use alexandria_http::app;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tempfile::tempdir;
use tower::ServiceExt;

use crate::common::{file_rows_with_uuid, test_app, wait_for_files};

fn index_request(root: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", "Bearer test-token")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "root": root }).to_string()))
        .unwrap()
}

fn edit_content_request(uuid: &str, content: &str) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(format!("/v1/files/{uuid}/content"))
        .header("authorization", "Bearer test-token")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "content": content }).to_string()))
        .unwrap()
}

fn unauthenticated_edit_content_request(uuid: &str, content: &str) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(format!("/v1/files/{uuid}/content"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "content": content }).to_string()))
        .unwrap()
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_text_file_when_content_edited_then_200_and_disk_and_hash_updated() {
    let lib = tempdir().unwrap();
    let path = common::write_file(&lib, "notes.txt", b"old content");

    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    router
        .clone()
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("index");
    wait_for_files(&test.pool, 1).await;
    let rows_before = file_rows_with_uuid(&test.pool).await;
    let uuid = rows_before[0].0.clone();
    let hash_before = rows_before[0].4.clone();

    let response = router
        .oneshot(edit_content_request(&uuid, "new content"))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["uuid"], uuid);
    assert_ne!(body["contentHash"], hash_before);

    let on_disk = std::fs::read_to_string(&path).expect("read back");
    assert_eq!(on_disk, "new content");

    let rows_after = file_rows_with_uuid(&test.pool).await;
    assert_eq!(rows_after[0].4, body["contentHash"].as_str().unwrap());
}

// ---------------- AF-01: invalid input (wrong file type) ----------------

#[tokio::test]
async fn given_non_text_file_when_content_edited_then_400_and_disk_unchanged() {
    let lib = tempdir().unwrap();
    let path = common::write_file(&lib, "song.mp3", b"audio bytes");

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

    let response = router
        .oneshot(edit_content_request(&uuid, "new content"))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_json(response).await["error"].is_string());
    let on_disk = std::fs::read(&path).expect("read back");
    assert_eq!(on_disk, b"audio bytes");
}

// ---------------- AF-04: not found ----------------

#[tokio::test]
async fn given_unknown_uuid_when_content_edited_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let unknown = uuid::Uuid::new_v4().to_string();
    let response = router
        .oneshot(edit_content_request(&unknown, "new content"))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------- AF-05: unauthorized ----------------

#[tokio::test]
async fn given_no_token_when_content_edited_then_401() {
    let lib = tempdir().unwrap();
    let path = common::write_file(&lib, "notes.txt", b"old content");

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

    let response = router
        .oneshot(unauthenticated_edit_content_request(&uuid, "new content"))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let on_disk = std::fs::read(&path).expect("read back");
    assert_eq!(on_disk, b"old content");
}
