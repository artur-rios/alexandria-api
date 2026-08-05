//! UC-30 integration tests for `DELETE /v1/reading-lists/{uuid}/items/{itemUuid}`
//! (Testing Specification §7): the real axum router over a real temp SQLite
//! database. Each test asserts both the response and the resulting persisted
//! state, and covers the main flow plus every alternative flow (AF-01 not
//! found, AF-02 unauthorized).

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

fn remove_item_request(reading_list_uuid: &str, item_uuid: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(format!(
            "/v1/reading-lists/{reading_list_uuid}/items/{item_uuid}"
        ))
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .body(Body::empty())
        .unwrap()
}

fn unauthenticated_remove_request(reading_list_uuid: &str, item_uuid: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(format!(
            "/v1/reading-lists/{reading_list_uuid}/items/{item_uuid}"
        ))
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

async fn seeded_linked(router: &axum::Router, pool: &SqlitePool) -> (String, String) {
    let create_resp = router
        .clone()
        .oneshot(create_reading_list_request("Summer reads"))
        .await
        .expect("create reading list");
    let reading_list_uuid = body_json(create_resp).await["uuid"]
        .as_str()
        .expect("uuid")
        .to_string();
    let item_uuid = seed_document(pool).await;
    let add_resp = router
        .clone()
        .oneshot(add_item_request(&reading_list_uuid, &item_uuid))
        .await
        .expect("add item");
    assert_eq!(add_resp.status(), StatusCode::OK);
    (reading_list_uuid, item_uuid)
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
async fn given_linked_item_when_removed_then_200_and_row_deleted_and_file_kept() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (reading_list_uuid, item_uuid) = seeded_linked(&router, &test.pool).await;

    let response = router
        .oneshot(remove_item_request(&reading_list_uuid, &item_uuid))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["readingListUuid"], reading_list_uuid);
    assert_eq!(body["itemUuid"], item_uuid);

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
async fn given_item_not_on_reading_list_when_removed_then_404() {
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
        .oneshot(remove_item_request(&reading_list_uuid, &unknown_item))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn given_already_removed_item_when_removed_again_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (reading_list_uuid, item_uuid) = seeded_linked(&router, &test.pool).await;

    let first = router
        .clone()
        .oneshot(remove_item_request(&reading_list_uuid, &item_uuid))
        .await
        .expect("first");
    assert_eq!(first.status(), StatusCode::OK);

    let second = router
        .oneshot(remove_item_request(&reading_list_uuid, &item_uuid))
        .await
        .expect("second");
    assert_eq!(second.status(), StatusCode::NOT_FOUND);
}

// ---------------- AF-02: unauthorized ----------------

#[tokio::test]
async fn given_no_token_when_removed_then_401_and_row_kept() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (reading_list_uuid, item_uuid) = seeded_linked(&router, &test.pool).await;

    let response = router
        .oneshot(unauthenticated_remove_request(
            &reading_list_uuid,
            &item_uuid,
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(progress_count(&test.pool).await, 1);
}
