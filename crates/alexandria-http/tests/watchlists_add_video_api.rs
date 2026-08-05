//! UC-22 integration tests for `POST /v1/watchlists/{uuid}/items` (Testing
//! Specification §7): the real axum router over a real temp SQLite database.
//! Each test asserts both the response and the resulting persisted state,
//! and covers the main flow plus every alternative flow (AF-01 wrong file
//! type, AF-02 watchlist/video not found, AF-03 unauthorized).

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

fn add_video_request(watchlist_uuid: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/v1/watchlists/{watchlist_uuid}/items"))
        .header("authorization", "Bearer test-token")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn unauthenticated_add_video_request(watchlist_uuid: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/v1/watchlists/{watchlist_uuid}/items"))
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

/// `(watchlist_id, video_file_id, state)` joined back to public uuids, for
/// every persisted watch_progress row.
async fn watch_progress_rows(pool: &SqlitePool) -> Vec<(String, String, String)> {
    sqlx::query_as(
        "SELECT w.uuid, f.uuid, wp.state \
         FROM watch_progress wp \
         JOIN watchlists w ON w.id = wp.watchlist_id \
         JOIN files f ON f.id = wp.video_file_id",
    )
    .fetch_all(pool)
    .await
    .expect("rows")
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_valid_video_when_posted_then_200_with_pending_progress_and_row_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let create_response = router
        .clone()
        .oneshot(create_watchlist_request("Weekend movies"))
        .await
        .expect("create watchlist");
    let watchlist = body_json(create_response).await;
    let watchlist_uuid = watchlist["uuid"].as_str().expect("watchlist uuid");

    let video_uuid = seed_file(&test.pool, "video").await;

    let response = router
        .oneshot(add_video_request(
            watchlist_uuid,
            json!({ "videoUuid": video_uuid }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["watchlistUuid"], watchlist_uuid);
    assert_eq!(body["videoUuid"], video_uuid);
    assert_eq!(body["state"], "pending");
    assert!(body["currentEpisode"].is_null());
    assert!(body["totalEpisodes"].is_null());

    let rows = watch_progress_rows(&test.pool).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, watchlist_uuid);
    assert_eq!(rows[0].1, video_uuid);
    assert_eq!(rows[0].2, "pending");
}

#[tokio::test]
async fn given_already_linked_video_when_posted_again_then_idempotent_no_duplicate_row() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let create_response = router
        .clone()
        .oneshot(create_watchlist_request("Weekend movies"))
        .await
        .expect("create watchlist");
    let watchlist_uuid = body_json(create_response).await["uuid"]
        .as_str()
        .expect("uuid")
        .to_string();
    let video_uuid = seed_file(&test.pool, "video").await;

    let first = router
        .clone()
        .oneshot(add_video_request(
            &watchlist_uuid,
            json!({ "videoUuid": video_uuid }),
        ))
        .await
        .expect("first");
    assert_eq!(first.status(), StatusCode::OK);

    let second = router
        .oneshot(add_video_request(
            &watchlist_uuid,
            json!({ "videoUuid": video_uuid }),
        ))
        .await
        .expect("second");
    assert_eq!(second.status(), StatusCode::OK);

    assert_eq!(watch_progress_rows(&test.pool).await.len(), 1);
}

// ---------------- AF-01: invalid input (wrong file type) ----------------

#[tokio::test]
async fn given_non_video_file_when_posted_then_400_and_nothing_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let create_response = router
        .clone()
        .oneshot(create_watchlist_request("Weekend movies"))
        .await
        .expect("create watchlist");
    let watchlist_uuid = body_json(create_response).await["uuid"]
        .as_str()
        .expect("uuid")
        .to_string();
    let audio_uuid = seed_file(&test.pool, "audio").await;

    let response = router
        .oneshot(add_video_request(
            &watchlist_uuid,
            json!({ "videoUuid": audio_uuid }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_json(response).await["error"].is_string());
    assert!(watch_progress_rows(&test.pool).await.is_empty());
}

// ---------------- AF-02: not found ----------------

#[tokio::test]
async fn given_unknown_watchlist_when_posted_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let video_uuid = seed_file(&test.pool, "video").await;
    let unknown = uuid::Uuid::new_v4().to_string();

    let response = router
        .oneshot(add_video_request(
            &unknown,
            json!({ "videoUuid": video_uuid }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(watch_progress_rows(&test.pool).await.is_empty());
}

#[tokio::test]
async fn given_unknown_video_when_posted_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let create_response = router
        .clone()
        .oneshot(create_watchlist_request("Weekend movies"))
        .await
        .expect("create watchlist");
    let watchlist_uuid = body_json(create_response).await["uuid"]
        .as_str()
        .expect("uuid")
        .to_string();
    let unknown = uuid::Uuid::new_v4().to_string();

    let response = router
        .oneshot(add_video_request(
            &watchlist_uuid,
            json!({ "videoUuid": unknown }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(watch_progress_rows(&test.pool).await.is_empty());
}

// ---------------- AF-03: unauthorized ----------------

#[tokio::test]
async fn given_no_token_when_posted_then_401_and_nothing_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let create_response = router
        .clone()
        .oneshot(create_watchlist_request("Weekend movies"))
        .await
        .expect("create watchlist");
    let watchlist_uuid = body_json(create_response).await["uuid"]
        .as_str()
        .expect("uuid")
        .to_string();
    let video_uuid = seed_file(&test.pool, "video").await;

    let response = router
        .oneshot(unauthenticated_add_video_request(
            &watchlist_uuid,
            json!({ "videoUuid": video_uuid }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(watch_progress_rows(&test.pool).await.is_empty());
}
