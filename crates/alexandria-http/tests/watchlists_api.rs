//! UC-20 integration tests for `POST /v1/watchlists` (Testing Specification
//! §7): the real axum router over a real temp SQLite database. Each test
//! asserts both the response and the resulting persisted state, and covers
//! the main flow plus every alternative flow (AF-01 invalid input, AF-02
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

/// `(uuid, name)` for every persisted watchlist, ordered by name.
async fn watchlist_rows(pool: &SqlitePool) -> Vec<(String, String)> {
    sqlx::query_as("SELECT uuid, name FROM watchlists ORDER BY name")
        .fetch_all(pool)
        .await
        .expect("rows")
}

fn create_request(body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/watchlists")
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn unauthenticated_request(body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/watchlists")
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
async fn given_valid_name_when_posted_then_201_with_watchlist_and_row_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(create_request(json!({ "name": "Weekend movies" })))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    assert_eq!(body["name"], "Weekend movies");
    let uuid = body["uuid"].as_str().expect("uuid string");
    assert!(uuid::Uuid::parse_str(uuid).is_ok(), "uuid is a valid UUID");

    let rows = watchlist_rows(&test.pool).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, uuid);
    assert_eq!(rows[0].1, "Weekend movies");
}

#[tokio::test]
async fn given_same_name_twice_when_posted_then_both_created_with_distinct_uuids() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let first = router
        .clone()
        .oneshot(create_request(json!({ "name": "Favorites" })))
        .await
        .expect("first");
    assert_eq!(first.status(), StatusCode::CREATED);
    let second = router
        .oneshot(create_request(json!({ "name": "Favorites" })))
        .await
        .expect("second");
    assert_eq!(second.status(), StatusCode::CREATED);

    assert_eq!(watchlist_rows(&test.pool).await.len(), 2);
}

// ---------------- AF-01: invalid input ----------------

#[tokio::test]
async fn given_empty_name_when_posted_then_400_and_nothing_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(create_request(json!({ "name": "" })))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_json(response).await["error"].is_string());
    assert!(watchlist_rows(&test.pool).await.is_empty());
}

#[tokio::test]
async fn given_missing_name_when_posted_then_400_and_nothing_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(create_request(json!({})))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(watchlist_rows(&test.pool).await.is_empty());
}

#[tokio::test]
async fn given_malformed_json_body_when_posted_then_400_with_error_envelope() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let request = Request::builder()
        .method("POST")
        .uri("/v1/watchlists")
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .header("content-type", "application/json")
        .body(Body::from("{ not json"))
        .unwrap();
    let response = router.oneshot(request).await.expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        body_json(response).await["error"].is_string(),
        "a parse failure uses this surface's error envelope, not axum's bare text"
    );
    assert!(watchlist_rows(&test.pool).await.is_empty());
}

// ---------------- AF-02: unauthorized ----------------

#[tokio::test]
async fn given_no_token_when_posted_then_401_and_nothing_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(unauthenticated_request(json!({ "name": "Weekend movies" })))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(watchlist_rows(&test.pool).await.is_empty());
}

#[tokio::test]
async fn given_no_token_and_malformed_body_when_posted_then_401_not_400() {
    // Authentication is evaluated before the body is parsed (FR-AU-07 / SRD
    // §7): an unauthenticated caller must not learn that its payload was
    // also unacceptable.
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let request = Request::builder()
        .method("POST")
        .uri("/v1/watchlists")
        .header("content-type", "application/json")
        .body(Body::from("{ not json"))
        .unwrap();
    let response = router.oneshot(request).await.expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(watchlist_rows(&test.pool).await.is_empty());
}
