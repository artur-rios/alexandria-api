//! UC-32 integration tests for `GET /v1/files/{uuid}/content` (Testing
//! Specification §7): the real axum router, a real temp SQLite database,
//! and a real on-disk file indexed through `POST /v1/index`. Each test
//! asserts the response, and covers the main flow plus every alternative
//! flow (AF-01 wrong file type, AF-03 not found, AF-04 unauthorized).

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
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .header("content-type", "application/json")
        .body(Body::from(json!({ "root": root }).to_string()))
        .unwrap()
}

fn get_content_request(uuid: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(format!("/v1/files/{uuid}/content"))
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .body(Body::empty())
        .unwrap()
}

fn unauthenticated_get_content_request(uuid: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(format!("/v1/files/{uuid}/content"))
        .body(Body::empty())
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
async fn given_text_file_when_content_requested_then_200_with_content() {
    let lib = tempdir().unwrap();
    common::write_file(&lib, "notes.txt", b"hello world");

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
        .oneshot(get_content_request(&uuid))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["uuid"], uuid);
    assert_eq!(body["content"], "hello world");
}

// ---------------- AF-01: invalid input (wrong file type) ----------------

#[tokio::test]
async fn given_non_text_file_when_content_requested_then_400() {
    let lib = tempdir().unwrap();
    common::write_file(&lib, "song.mp3", b"audio bytes");

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
        .oneshot(get_content_request(&uuid))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_json(response).await["error"].is_string());
}

// ---------------- AF-03: not found ----------------

#[tokio::test]
async fn given_unknown_uuid_when_content_requested_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let unknown = uuid::Uuid::new_v4().to_string();
    let response = router
        .oneshot(get_content_request(&unknown))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------- AF-04: unauthorized ----------------

#[tokio::test]
async fn given_no_token_when_content_requested_then_401() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let unknown = uuid::Uuid::new_v4().to_string();
    let response = router
        .oneshot(unauthenticated_get_content_request(&unknown))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
