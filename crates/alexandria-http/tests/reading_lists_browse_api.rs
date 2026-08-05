//! UC-27 integration tests for `GET /v1/reading-lists` (Testing
//! Specification §7): the real axum router over a real temp SQLite
//! database. Each test asserts the response body, and covers the main flow
//! (all reading lists, single reading list) plus every alternative flow
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

fn list_request(query: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(format!("/v1/reading-lists{query}"))
        .header("authorization", "Bearer test-token")
        .body(Body::empty())
        .unwrap()
}

fn unauthenticated_list_request() -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri("/v1/reading-lists")
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

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_no_filter_when_listed_then_200_with_every_reading_list_and_progress() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let create_a = router
        .clone()
        .oneshot(create_reading_list_request("A list"))
        .await
        .expect("create a");
    let a_uuid = body_json(create_a).await["uuid"]
        .as_str()
        .expect("uuid")
        .to_string();
    router
        .clone()
        .oneshot(create_reading_list_request("B list"))
        .await
        .expect("create b");

    let item_uuid = seed_document(&test.pool).await;
    let add_resp = router
        .clone()
        .oneshot(add_item_request(&a_uuid, &item_uuid))
        .await
        .expect("add item");
    assert_eq!(add_resp.status(), StatusCode::OK);

    let response = router.oneshot(list_request("")).await.expect("list");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let reading_lists = body.as_array().expect("array");
    assert_eq!(reading_lists.len(), 2);

    let a = reading_lists
        .iter()
        .find(|w| w["uuid"] == a_uuid)
        .expect("a list present");
    let items = a["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["itemUuid"], item_uuid);
    assert_eq!(items[0]["targetKind"], "document");
    assert_eq!(items[0]["state"], "pending");

    let b = reading_lists
        .iter()
        .find(|w| w["name"] == "B list")
        .expect("b list present");
    assert!(b["items"].as_array().expect("items array").is_empty());
}

#[tokio::test]
async fn given_reading_list_uuid_filter_when_listed_then_200_with_only_that_reading_list() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let create_a = router
        .clone()
        .oneshot(create_reading_list_request("A list"))
        .await
        .expect("create a");
    let a_uuid = body_json(create_a).await["uuid"]
        .as_str()
        .expect("uuid")
        .to_string();
    router
        .clone()
        .oneshot(create_reading_list_request("B list"))
        .await
        .expect("create b");

    let response = router
        .oneshot(list_request(&format!("?readingListUuid={a_uuid}")))
        .await
        .expect("list");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let reading_lists = body.as_array().expect("array");
    assert_eq!(reading_lists.len(), 1);
    assert_eq!(reading_lists[0]["uuid"], a_uuid);
}

#[tokio::test]
async fn given_no_reading_lists_when_listed_then_200_with_empty_array() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router.oneshot(list_request("")).await.expect("list");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await.as_array().unwrap().len(), 0);
}

// ---------------- AF-01: not found ----------------

#[tokio::test]
async fn given_unknown_reading_list_uuid_when_listed_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let unknown = uuid::Uuid::new_v4().to_string();
    let response = router
        .oneshot(list_request(&format!("?readingListUuid={unknown}")))
        .await
        .expect("list");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------- AF-02: unauthorized ----------------

#[tokio::test]
async fn given_no_token_when_listed_then_401() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(unauthenticated_list_request())
        .await
        .expect("list");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
