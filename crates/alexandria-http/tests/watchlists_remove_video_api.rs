//! UC-24 integration tests for `DELETE /v1/watchlists/{uuid}/items/{videoUuid}`
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

fn create_watchlist_request(name: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/watchlists")
        .header("authorization", "Bearer test-token")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "name": name }).to_string()))
        .unwrap()
}

fn add_video_request(watchlist_uuid: &str, video_uuid: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/v1/watchlists/{watchlist_uuid}/items"))
        .header("authorization", "Bearer test-token")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "videoUuid": video_uuid }).to_string()))
        .unwrap()
}

fn remove_video_request(watchlist_uuid: &str, video_uuid: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(format!(
            "/v1/watchlists/{watchlist_uuid}/items/{video_uuid}"
        ))
        .header("authorization", "Bearer test-token")
        .body(Body::empty())
        .unwrap()
}

fn unauthenticated_remove_request(watchlist_uuid: &str, video_uuid: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(format!(
            "/v1/watchlists/{watchlist_uuid}/items/{video_uuid}"
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

async fn seeded_linked(router: &axum::Router, pool: &SqlitePool) -> (String, String) {
    let create_resp = router
        .clone()
        .oneshot(create_watchlist_request("Weekend movies"))
        .await
        .expect("create watchlist");
    let watchlist_uuid = body_json(create_resp).await["uuid"]
        .as_str()
        .expect("uuid")
        .to_string();
    let video_uuid = seed_video(pool).await;
    let add_resp = router
        .clone()
        .oneshot(add_video_request(&watchlist_uuid, &video_uuid))
        .await
        .expect("add video");
    assert_eq!(add_resp.status(), StatusCode::OK);
    (watchlist_uuid, video_uuid)
}

async fn progress_count(pool: &SqlitePool) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM watch_progress")
        .fetch_one(pool)
        .await
        .expect("count");
    row.0
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_linked_video_when_removed_then_200_and_row_deleted_and_file_kept() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (watchlist_uuid, video_uuid) = seeded_linked(&router, &test.pool).await;

    let response = router
        .oneshot(remove_video_request(&watchlist_uuid, &video_uuid))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["watchlistUuid"], watchlist_uuid);
    assert_eq!(body["videoUuid"], video_uuid);

    assert_eq!(progress_count(&test.pool).await, 0);
    let file_rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files WHERE uuid = ?")
        .bind(&video_uuid)
        .fetch_one(&test.pool)
        .await
        .expect("file row");
    assert_eq!(file_rows.0, 1, "the VideoFile itself is preserved");
}

// ---------------- AF-01: not found ----------------

#[tokio::test]
async fn given_video_not_on_watchlist_when_removed_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let create_resp = router
        .clone()
        .oneshot(create_watchlist_request("Weekend movies"))
        .await
        .expect("create watchlist");
    let watchlist_uuid = body_json(create_resp).await["uuid"]
        .as_str()
        .expect("uuid")
        .to_string();
    let unknown_video = uuid::Uuid::new_v4().to_string();

    let response = router
        .oneshot(remove_video_request(&watchlist_uuid, &unknown_video))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn given_already_removed_video_when_removed_again_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (watchlist_uuid, video_uuid) = seeded_linked(&router, &test.pool).await;

    let first = router
        .clone()
        .oneshot(remove_video_request(&watchlist_uuid, &video_uuid))
        .await
        .expect("first");
    assert_eq!(first.status(), StatusCode::OK);

    let second = router
        .oneshot(remove_video_request(&watchlist_uuid, &video_uuid))
        .await
        .expect("second");
    assert_eq!(second.status(), StatusCode::NOT_FOUND);
}

// ---------------- AF-02: unauthorized ----------------

#[tokio::test]
async fn given_no_token_when_removed_then_401_and_row_kept() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (watchlist_uuid, video_uuid) = seeded_linked(&router, &test.pool).await;

    let response = router
        .oneshot(unauthenticated_remove_request(&watchlist_uuid, &video_uuid))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(progress_count(&test.pool).await, 1);
}
