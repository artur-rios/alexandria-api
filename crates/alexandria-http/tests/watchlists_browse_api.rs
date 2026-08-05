//! UC-21 integration tests for `GET /v1/watchlists` (Testing Specification
//! §7): the real axum router over a real temp SQLite database. Each test
//! asserts the response body, and covers the main flow (all watchlists,
//! single watchlist) plus every alternative flow (AF-01 not found, AF-02
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

fn create_watchlist_request(name: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/watchlists")
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .header("content-type", "application/json")
        .body(Body::from(json!({ "name": name }).to_string()))
        .unwrap()
}

fn add_video_request(watchlist_uuid: &str, video_uuid: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/v1/watchlists/{watchlist_uuid}/items"))
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .header("content-type", "application/json")
        .body(Body::from(json!({ "videoUuid": video_uuid }).to_string()))
        .unwrap()
}

fn list_request(query: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(format!("/v1/watchlists{query}"))
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .body(Body::empty())
        .unwrap()
}

fn unauthenticated_list_request() -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri("/v1/watchlists")
        .body(Body::empty())
        .unwrap()
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

async fn seed_video(pool: &SqlitePool) -> String {
    let file_uuid = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO files (uuid, path, name, type, content_hash, indexed_at) \
         VALUES (?, ?, ?, 'video', 'hash', ?)",
    )
    .bind(&file_uuid)
    .bind(format!("/lib/{file_uuid}"))
    .bind("seeded")
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(pool)
    .await
    .expect("seed video");
    file_uuid
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_no_filter_when_listed_then_200_with_every_watchlist_and_progress() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let create_a = router
        .clone()
        .oneshot(create_watchlist_request("A list"))
        .await
        .expect("create a");
    let a_uuid = body_json(create_a).await["uuid"]
        .as_str()
        .expect("uuid")
        .to_string();
    router
        .clone()
        .oneshot(create_watchlist_request("B list"))
        .await
        .expect("create b");

    let video_uuid = seed_video(&test.pool).await;
    let add_resp = router
        .clone()
        .oneshot(add_video_request(&a_uuid, &video_uuid))
        .await
        .expect("add video");
    assert_eq!(add_resp.status(), StatusCode::OK);

    let response = router.oneshot(list_request("")).await.expect("list");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let watchlists = body.as_array().expect("array");
    assert_eq!(watchlists.len(), 2);

    let a = watchlists
        .iter()
        .find(|w| w["uuid"] == a_uuid)
        .expect("a list present");
    let items = a["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["videoUuid"], video_uuid);
    assert_eq!(items[0]["state"], "pending");

    let b = watchlists
        .iter()
        .find(|w| w["name"] == "B list")
        .expect("b list present");
    assert!(b["items"].as_array().expect("items array").is_empty());
}

#[tokio::test]
async fn given_watchlist_uuid_filter_when_listed_then_200_with_only_that_watchlist() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let create_a = router
        .clone()
        .oneshot(create_watchlist_request("A list"))
        .await
        .expect("create a");
    let a_uuid = body_json(create_a).await["uuid"]
        .as_str()
        .expect("uuid")
        .to_string();
    router
        .clone()
        .oneshot(create_watchlist_request("B list"))
        .await
        .expect("create b");

    let response = router
        .oneshot(list_request(&format!("?watchlistUuid={a_uuid}")))
        .await
        .expect("list");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let watchlists = body.as_array().expect("array");
    assert_eq!(watchlists.len(), 1);
    assert_eq!(watchlists[0]["uuid"], a_uuid);
}

#[tokio::test]
async fn given_no_watchlists_when_listed_then_200_with_empty_array() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router.oneshot(list_request("")).await.expect("list");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await.as_array().unwrap().len(), 0);
}

// ---------------- AF-01: not found ----------------

#[tokio::test]
async fn given_unknown_watchlist_uuid_when_listed_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let unknown = uuid::Uuid::new_v4().to_string();
    let response = router
        .oneshot(list_request(&format!("?watchlistUuid={unknown}")))
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
