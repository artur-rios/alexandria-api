//! UC-23 integration tests for `PATCH /v1/watchlists/{uuid}/items/{videoUuid}`
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

fn update_progress_request(watchlist_uuid: &str, video_uuid: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("PATCH")
        .uri(format!(
            "/v1/watchlists/{watchlist_uuid}/items/{video_uuid}"
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
    watchlist_uuid: &str,
    video_uuid: &str,
    body: Value,
) -> Request<Body> {
    Request::builder()
        .method("PATCH")
        .uri(format!(
            "/v1/watchlists/{watchlist_uuid}/items/{video_uuid}"
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

/// Set up a watchlist with one `Pending` video linked, returning
/// `(watchlist_uuid, video_uuid)`.
async fn seeded_pending(router: &axum::Router, pool: &SqlitePool) -> (String, String) {
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

async fn progress_state(pool: &SqlitePool) -> String {
    let row: (String,) = sqlx::query_as("SELECT state FROM watch_progress")
        .fetch_one(pool)
        .await
        .expect("row");
    row.0
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_pending_when_updated_to_watching_then_200_and_row_updated() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (watchlist_uuid, video_uuid) = seeded_pending(&router, &test.pool).await;

    let response = router
        .oneshot(update_progress_request(
            &watchlist_uuid,
            &video_uuid,
            json!({ "state": "watching" }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["watchlistUuid"], watchlist_uuid);
    assert_eq!(body["videoUuid"], video_uuid);
    assert_eq!(body["state"], "watching");
    assert_eq!(progress_state(&test.pool).await, "watching");
}

#[tokio::test]
async fn given_series_episode_when_updated_then_200_and_episode_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (watchlist_uuid, video_uuid) = seeded_pending(&router, &test.pool).await;

    let response = router
        .oneshot(update_progress_request(
            &watchlist_uuid,
            &video_uuid,
            json!({ "state": "watching", "currentEpisode": 3, "totalEpisodes": 12 }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["currentEpisode"], 3);
    assert_eq!(body["totalEpisodes"], 12);

    let row: (Option<i64>, Option<i64>) =
        sqlx::query_as("SELECT current_episode, total_episodes FROM watch_progress")
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
    let (watchlist_uuid, video_uuid) = seeded_pending(&router, &test.pool).await;

    let response = router
        .oneshot(update_progress_request(
            &watchlist_uuid,
            &video_uuid,
            json!({ "state": "watched" }),
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
    let (watchlist_uuid, video_uuid) = seeded_pending(&router, &test.pool).await;

    let response = router
        .oneshot(update_progress_request(
            &watchlist_uuid,
            &video_uuid,
            json!({ "state": "paused" }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(progress_state(&test.pool).await, "pending");
}

// ---------------- AF-02: not found ----------------

#[tokio::test]
async fn given_video_not_on_watchlist_when_updated_then_404() {
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
        .oneshot(update_progress_request(
            &watchlist_uuid,
            &unknown_video,
            json!({ "state": "watching" }),
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
    let (watchlist_uuid, video_uuid) = seeded_pending(&router, &test.pool).await;

    let response = router
        .oneshot(unauthenticated_update_request(
            &watchlist_uuid,
            &video_uuid,
            json!({ "state": "watching" }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(progress_state(&test.pool).await, "pending");
}
