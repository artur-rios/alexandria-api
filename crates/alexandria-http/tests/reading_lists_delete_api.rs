//! UC-31 integration tests for `DELETE /v1/reading-lists/{uuid}` (Testing
//! Specification §7): the real axum router over a real temp SQLite
//! database. Each test asserts both the response and the resulting
//! persisted state, and covers the main flow plus every alternative flow
//! (AF-01 not found, AF-02 unauthorized).

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
        .header("authorization", "Bearer test-token")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "name": name }).to_string()))
        .unwrap()
}

fn add_item_request(reading_list_uuid: &str, item_uuid: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/v1/reading-lists/{reading_list_uuid}/items"))
        .header("authorization", "Bearer test-token")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "itemUuid": item_uuid }).to_string()))
        .unwrap()
}

fn delete_request(reading_list_uuid: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(format!("/v1/reading-lists/{reading_list_uuid}"))
        .header("authorization", "Bearer test-token")
        .body(Body::empty())
        .unwrap()
}

fn unauthenticated_delete_request(reading_list_uuid: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(format!("/v1/reading-lists/{reading_list_uuid}"))
        .body(Body::empty())
        .unwrap()
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

async fn seed_document(pool: &SqlitePool) -> String {
    let file_uuid = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO files (uuid, path, name, type, content_hash, indexed_at) \
         VALUES (?, ?, ?, 'document', 'hash', ?)",
    )
    .bind(&file_uuid)
    .bind(format!("/lib/{file_uuid}"))
    .bind("seeded")
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(pool)
    .await
    .expect("seed document");
    file_uuid
}

async fn reading_list_count(pool: &SqlitePool) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM reading_lists")
        .fetch_one(pool)
        .await
        .expect("count");
    row.0
}

async fn progress_count(pool: &SqlitePool) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM reading_progress")
        .fetch_one(pool)
        .await
        .expect("count");
    row.0
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_empty_reading_list_when_deleted_then_200_with_predelete_body_and_row_removed() {
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

    let response = router
        .oneshot(delete_request(&reading_list_uuid))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["uuid"], reading_list_uuid);
    assert_eq!(body["name"], "Summer reads");

    assert_eq!(reading_list_count(&test.pool).await, 0);
}

#[tokio::test]
async fn given_reading_list_with_linked_item_when_deleted_then_progress_gone_and_item_preserved() {
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
    let item_uuid = seed_document(&test.pool).await;
    let add_resp = router
        .clone()
        .oneshot(add_item_request(&reading_list_uuid, &item_uuid))
        .await
        .expect("add item");
    assert_eq!(add_resp.status(), StatusCode::OK);

    let response = router
        .oneshot(delete_request(&reading_list_uuid))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(reading_list_count(&test.pool).await, 0);
    assert_eq!(progress_count(&test.pool).await, 0);
    let file_rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files WHERE uuid = ?")
        .bind(&item_uuid)
        .fetch_one(&test.pool)
        .await
        .expect("file row");
    assert_eq!(file_rows.0, 1, "the file itself is preserved");
}

// ---------------- AF-01: not found ----------------

#[tokio::test]
async fn given_unknown_uuid_when_deleted_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let unknown = uuid::Uuid::new_v4().to_string();
    let response = router
        .oneshot(delete_request(&unknown))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------- AF-02: unauthorized ----------------

#[tokio::test]
async fn given_no_token_when_deleted_then_401_and_reading_list_kept() {
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

    let response = router
        .oneshot(unauthenticated_delete_request(&reading_list_uuid))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(reading_list_count(&test.pool).await, 1);
}
