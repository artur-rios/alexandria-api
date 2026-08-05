//! UC-29 integration tests for `PATCH /v1/reading-lists/{uuid}/items/{itemUuid}`
//! (Testing Specification §7): the real axum router over a real temp SQLite
//! database. Each test asserts both the response and the resulting persisted
//! state, and covers the main flow plus every alternative flow (AF-01
//! invalid transition, AF-02 not found, AF-03 unauthorized).

mod common;

use alexandria_core::config::Settings;
use alexandria_http::app;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use sqlx::sqlite::SqlitePool;
use tower::ServiceExt;

use crate::common::test_app;

fn create_reading_list_request(name: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/reading-lists")
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .header("content-type", "application/json")
        .body(Body::from(json!({ "name": name }).to_string()))
        .unwrap()
}

fn add_item_request(reading_list_uuid: &str, item_uuid: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/v1/reading-lists/{reading_list_uuid}/items"))
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .header("content-type", "application/json")
        .body(Body::from(json!({ "itemUuid": item_uuid }).to_string()))
        .unwrap()
}

fn update_progress_request(reading_list_uuid: &str, item_uuid: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("PATCH")
        .uri(format!(
            "/v1/reading-lists/{reading_list_uuid}/items/{item_uuid}"
        ))
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn unauthenticated_update_request(
    reading_list_uuid: &str,
    item_uuid: &str,
    body: Value,
) -> Request<Body> {
    Request::builder()
        .method("PATCH")
        .uri(format!(
            "/v1/reading-lists/{reading_list_uuid}/items/{item_uuid}"
        ))
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

async fn seed_comic(pool: &SqlitePool) -> String {
    let file_uuid = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO files (uuid, path, name, type, content_hash, indexed_at) \
         VALUES (?, ?, ?, 'comic', 'hash', ?)",
    )
    .bind(&file_uuid)
    .bind(format!("/lib/{file_uuid}"))
    .bind("seeded")
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(pool)
    .await
    .expect("seed comic");
    file_uuid
}

/// Set up a reading list with one `Pending` comic linked, returning
/// `(reading_list_uuid, item_uuid)`.
async fn seeded_pending(router: &axum::Router, pool: &SqlitePool) -> (String, String) {
    let create_resp = router
        .clone()
        .oneshot(create_reading_list_request("Summer reads"))
        .await
        .expect("create reading list");
    let reading_list_uuid = body_json(create_resp).await["uuid"]
        .as_str()
        .expect("uuid")
        .to_string();
    let item_uuid = seed_comic(pool).await;
    let add_resp = router
        .clone()
        .oneshot(add_item_request(&reading_list_uuid, &item_uuid))
        .await
        .expect("add item");
    assert_eq!(add_resp.status(), StatusCode::OK);
    (reading_list_uuid, item_uuid)
}

async fn progress_state(pool: &SqlitePool) -> String {
    let row: (String,) = sqlx::query_as("SELECT state FROM reading_progress")
        .fetch_one(pool)
        .await
        .expect("row");
    row.0
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_pending_when_updated_to_reading_then_200_and_row_updated() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (reading_list_uuid, item_uuid) = seeded_pending(&router, &test.pool).await;

    let response = router
        .oneshot(update_progress_request(
            &reading_list_uuid,
            &item_uuid,
            json!({ "state": "reading" }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["readingListUuid"], reading_list_uuid);
    assert_eq!(body["itemUuid"], item_uuid);
    assert_eq!(body["state"], "reading");
    assert_eq!(progress_state(&test.pool).await, "reading");
}

#[tokio::test]
async fn given_comic_issue_when_updated_then_200_and_issue_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (reading_list_uuid, item_uuid) = seeded_pending(&router, &test.pool).await;

    let response = router
        .oneshot(update_progress_request(
            &reading_list_uuid,
            &item_uuid,
            json!({ "state": "reading", "currentIssue": 3, "totalIssues": 12 }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["currentIssue"], 3);
    assert_eq!(body["totalIssues"], 12);

    let row: (Option<i64>, Option<i64>) =
        sqlx::query_as("SELECT current_issue, total_issues FROM reading_progress")
            .fetch_one(&test.pool)
            .await
            .expect("row");
    assert_eq!(row, (Some(3), Some(12)));
}

// ---------------- AF-01: invalid transition ----------------

#[tokio::test]
async fn given_backward_transition_when_updated_then_409_and_row_unchanged() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (reading_list_uuid, item_uuid) = seeded_pending(&router, &test.pool).await;

    let response = router
        .oneshot(update_progress_request(
            &reading_list_uuid,
            &item_uuid,
            json!({ "state": "read" }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(progress_state(&test.pool).await, "pending");
}

#[tokio::test]
async fn given_unknown_state_when_updated_then_400() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (reading_list_uuid, item_uuid) = seeded_pending(&router, &test.pool).await;

    let response = router
        .oneshot(update_progress_request(
            &reading_list_uuid,
            &item_uuid,
            json!({ "state": "paused" }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(progress_state(&test.pool).await, "pending");
}

// ---------------- AF-02: not found ----------------

#[tokio::test]
async fn given_item_not_on_reading_list_when_updated_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let create_resp = router
        .clone()
        .oneshot(create_reading_list_request("Summer reads"))
        .await
        .expect("create reading list");
    let reading_list_uuid = body_json(create_resp).await["uuid"]
        .as_str()
        .expect("uuid")
        .to_string();
    let unknown_item = uuid::Uuid::new_v4().to_string();

    let response = router
        .oneshot(update_progress_request(
            &reading_list_uuid,
            &unknown_item,
            json!({ "state": "reading" }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------- AF-03: unauthorized ----------------

#[tokio::test]
async fn given_no_token_when_updated_then_401_and_row_unchanged() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (reading_list_uuid, item_uuid) = seeded_pending(&router, &test.pool).await;

    let response = router
        .oneshot(unauthenticated_update_request(
            &reading_list_uuid,
            &item_uuid,
            json!({ "state": "reading" }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(progress_state(&test.pool).await, "pending");
}
