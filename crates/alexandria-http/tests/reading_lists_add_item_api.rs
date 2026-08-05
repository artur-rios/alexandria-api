//! UC-28 integration tests for `POST /v1/reading-lists/{uuid}/items`
//! (Testing Specification §7): the real axum router over a real temp SQLite
//! database. Each test asserts both the response and the resulting
//! persisted state, and covers the main flow plus every alternative flow
//! (AF-01 ineligible type, AF-02 reading list/item not found, AF-03
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

fn unauthenticated_add_item_request(reading_list_uuid: &str, item_uuid: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/v1/reading-lists/{reading_list_uuid}/items"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "itemUuid": item_uuid }).to_string()))
        .unwrap()
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

/// Insert a minimal `files` row of the given `file_type` and return its uuid.
async fn seed_file(pool: &SqlitePool, file_type: &str) -> String {
    let file_uuid = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO files (uuid, path, name, type, content_hash, indexed_at) \
         VALUES (?, ?, ?, ?, 'hash', ?)",
    )
    .bind(&file_uuid)
    .bind(format!("/lib/{file_uuid}"))
    .bind("seeded")
    .bind(file_type)
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(pool)
    .await
    .expect("seed file");
    file_uuid
}

/// `(reading_list_id, item_file_id, target_kind, state)` joined back to
/// public uuids, for every persisted reading_progress row.
async fn reading_progress_rows(pool: &SqlitePool) -> Vec<(String, String, String, String)> {
    sqlx::query_as(
        "SELECT rl.uuid, f.uuid, rp.target_kind, rp.state \
         FROM reading_progress rp \
         JOIN reading_lists rl ON rl.id = rp.reading_list_id \
         JOIN files f ON f.id = rp.item_file_id",
    )
    .fetch_all(pool)
    .await
    .expect("rows")
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_document_when_posted_then_200_with_pending_progress_and_row_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let create_response = router
        .clone()
        .oneshot(create_reading_list_request("Summer reads"))
        .await
        .expect("create reading list");
    let reading_list_uuid = body_json(create_response).await["uuid"]
        .as_str()
        .expect("reading list uuid")
        .to_string();

    let item_uuid = seed_file(&test.pool, "document").await;

    let response = router
        .oneshot(add_item_request(&reading_list_uuid, &item_uuid))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["readingListUuid"], reading_list_uuid);
    assert_eq!(body["itemUuid"], item_uuid);
    assert_eq!(body["targetKind"], "document");
    assert_eq!(body["state"], "pending");
    assert!(body["currentIssue"].is_null());
    assert!(body["totalIssues"].is_null());

    let rows = reading_progress_rows(&test.pool).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, reading_list_uuid);
    assert_eq!(rows[0].1, item_uuid);
    assert_eq!(rows[0].2, "document");
    assert_eq!(rows[0].3, "pending");
}

#[tokio::test]
async fn given_comic_when_posted_then_target_kind_comic_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let create_response = router
        .clone()
        .oneshot(create_reading_list_request("Summer reads"))
        .await
        .expect("create reading list");
    let reading_list_uuid = body_json(create_response).await["uuid"]
        .as_str()
        .expect("uuid")
        .to_string();
    let item_uuid = seed_file(&test.pool, "comic").await;

    let response = router
        .oneshot(add_item_request(&reading_list_uuid, &item_uuid))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["targetKind"], "comic");
}

#[tokio::test]
async fn given_already_linked_item_when_posted_again_then_idempotent_no_duplicate_row() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let create_response = router
        .clone()
        .oneshot(create_reading_list_request("Summer reads"))
        .await
        .expect("create reading list");
    let reading_list_uuid = body_json(create_response).await["uuid"]
        .as_str()
        .expect("uuid")
        .to_string();
    let item_uuid = seed_file(&test.pool, "document").await;

    let first = router
        .clone()
        .oneshot(add_item_request(&reading_list_uuid, &item_uuid))
        .await
        .expect("first");
    assert_eq!(first.status(), StatusCode::OK);

    let second = router
        .oneshot(add_item_request(&reading_list_uuid, &item_uuid))
        .await
        .expect("second");
    assert_eq!(second.status(), StatusCode::OK);

    assert_eq!(reading_progress_rows(&test.pool).await.len(), 1);
}

// ---------------- AF-01: invalid input (ineligible type) ----------------

#[tokio::test]
async fn given_ineligible_file_when_posted_then_400_and_nothing_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let create_response = router
        .clone()
        .oneshot(create_reading_list_request("Summer reads"))
        .await
        .expect("create reading list");
    let reading_list_uuid = body_json(create_response).await["uuid"]
        .as_str()
        .expect("uuid")
        .to_string();
    let video_uuid = seed_file(&test.pool, "video").await;

    let response = router
        .oneshot(add_item_request(&reading_list_uuid, &video_uuid))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_json(response).await["error"].is_string());
    assert!(reading_progress_rows(&test.pool).await.is_empty());
}

// ---------------- AF-02: not found ----------------

#[tokio::test]
async fn given_unknown_reading_list_when_posted_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let item_uuid = seed_file(&test.pool, "document").await;
    let unknown = uuid::Uuid::new_v4().to_string();

    let response = router
        .oneshot(add_item_request(&unknown, &item_uuid))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(reading_progress_rows(&test.pool).await.is_empty());
}

#[tokio::test]
async fn given_unknown_item_when_posted_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let create_response = router
        .clone()
        .oneshot(create_reading_list_request("Summer reads"))
        .await
        .expect("create reading list");
    let reading_list_uuid = body_json(create_response).await["uuid"]
        .as_str()
        .expect("uuid")
        .to_string();
    let unknown = uuid::Uuid::new_v4().to_string();

    let response = router
        .oneshot(add_item_request(&reading_list_uuid, &unknown))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(reading_progress_rows(&test.pool).await.is_empty());
}

// ---------------- AF-03: unauthorized ----------------

#[tokio::test]
async fn given_no_token_when_posted_then_401_and_nothing_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let create_response = router
        .clone()
        .oneshot(create_reading_list_request("Summer reads"))
        .await
        .expect("create reading list");
    let reading_list_uuid = body_json(create_response).await["uuid"]
        .as_str()
        .expect("uuid")
        .to_string();
    let item_uuid = seed_file(&test.pool, "document").await;

    let response = router
        .oneshot(unauthenticated_add_item_request(
            &reading_list_uuid,
            &item_uuid,
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(reading_progress_rows(&test.pool).await.is_empty());
}
