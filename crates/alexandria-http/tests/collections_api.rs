//! UC-10 integration tests for `POST /v1/collections` (Testing Specification
//! §7): the real axum router over a real temp SQLite database. Each test
//! asserts both the response and the resulting persisted state, and covers the
//! main flow plus every alternative flow (AF-01 invalid input, AF-02
//! unauthorized).

mod common;

use alexandria_core::config::Settings;
use alexandria_http::app;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use sqlx::sqlite::SqlitePool;
use tower::ServiceExt;

use crate::common::test_app;

/// `(uuid, name, kind)` for every persisted collection, ordered by name.
async fn collection_rows(pool: &SqlitePool) -> Vec<(String, String, String)> {
    sqlx::query_as("SELECT uuid, name, kind FROM collections ORDER BY name")
        .fetch_all(pool)
        .await
        .expect("rows")
}

fn create_request(body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/collections")
        .header("authorization", "Bearer test-token")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// The same request with no `Authorization` header (AF-02).
fn unauthenticated_request(body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/collections")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
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
async fn given_valid_file_collection_when_posted_then_201_with_collection_and_row_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(create_request(
            json!({ "name": "Sci-fi novels", "kind": "file" }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    assert_eq!(body["name"], "Sci-fi novels");
    assert_eq!(body["kind"], "file");
    let uuid = body["uuid"].as_str().expect("uuid string");
    assert!(uuid::Uuid::parse_str(uuid).is_ok(), "uuid is a valid UUID");

    let rows = collection_rows(&test.pool).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, uuid, "the returned uuid is the persisted one");
    assert_eq!(rows[0].1, "Sci-fi novels");
    assert_eq!(rows[0].2, "file");
}

#[tokio::test]
async fn given_valid_bookmark_collection_when_posted_then_kind_persisted_as_bookmark() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(create_request(
            json!({ "name": "Rust reading", "kind": "bookmark" }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(body_json(response).await["kind"], "bookmark");

    let rows = collection_rows(&test.pool).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].2, "bookmark");
}

#[tokio::test]
async fn given_same_name_twice_when_posted_then_both_created_with_distinct_uuids() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let first = router
        .clone()
        .oneshot(create_request(
            json!({ "name": "Favorites", "kind": "file" }),
        ))
        .await
        .expect("first");
    assert_eq!(first.status(), StatusCode::CREATED);
    let second = router
        .oneshot(create_request(
            json!({ "name": "Favorites", "kind": "file" }),
        ))
        .await
        .expect("second");
    assert_eq!(second.status(), StatusCode::CREATED);

    let first_uuid = body_json(first).await["uuid"].as_str().unwrap().to_string();
    let second_uuid = body_json(second).await["uuid"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(first_uuid, second_uuid);
    assert_eq!(collection_rows(&test.pool).await.len(), 2);
}

// ---------------- AF-01: invalid input ----------------

#[tokio::test]
async fn given_empty_name_when_posted_then_400_and_nothing_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(create_request(json!({ "name": "", "kind": "file" })))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_json(response).await["error"].is_string());
    assert!(collection_rows(&test.pool).await.is_empty());
}

#[tokio::test]
async fn given_blank_name_when_posted_then_400_and_nothing_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(create_request(json!({ "name": "   ", "kind": "file" })))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(collection_rows(&test.pool).await.is_empty());
}

#[tokio::test]
async fn given_unrecognised_kind_when_posted_then_400_and_nothing_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(create_request(
            json!({ "name": "Mixed bag", "kind": "playlist" }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_json(response).await["error"].is_string());
    assert!(collection_rows(&test.pool).await.is_empty());
}

#[tokio::test]
async fn given_missing_kind_when_posted_then_400_and_nothing_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(create_request(json!({ "name": "Sci-fi novels" })))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(collection_rows(&test.pool).await.is_empty());
}

#[tokio::test]
async fn given_malformed_json_body_when_posted_then_400_with_error_envelope() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let request = Request::builder()
        .method("POST")
        .uri("/v1/collections")
        .header("authorization", "Bearer test-token")
        .header("content-type", "application/json")
        .body(Body::from("{ not json"))
        .unwrap();
    let response = router.oneshot(request).await.expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        body_json(response).await["error"].is_string(),
        "a parse failure uses this surface's error envelope, not axum's bare text"
    );
    assert!(collection_rows(&test.pool).await.is_empty());
}

// ---------------- AF-02: unauthorized ----------------

#[tokio::test]
async fn given_no_token_when_posted_then_401_and_nothing_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(unauthenticated_request(
            json!({ "name": "Sci-fi novels", "kind": "file" }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(collection_rows(&test.pool).await.is_empty());
}

#[tokio::test]
async fn given_no_token_and_malformed_body_when_posted_then_401_not_400() {
    // Authentication is evaluated before the body is parsed (FR-AU-07 / SRD
    // §7): an unauthenticated caller must not learn that its payload was also
    // unacceptable.
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let request = Request::builder()
        .method("POST")
        .uri("/v1/collections")
        .header("content-type", "application/json")
        .body(Body::from("{ not json"))
        .unwrap();
    let response = router.oneshot(request).await.expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(collection_rows(&test.pool).await.is_empty());
}
