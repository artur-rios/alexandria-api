//! HTTP integration tests for the play history surface (`/v1/plays` and
//! `/v1/plays/stats`): the real axum router over a real temp SQLite
//! database. One test per route, plus the status mappings that are easy to
//! get wrong -- 401 before the body is parsed, 400 for a file that is not
//! audio and for an out-of-range limit, and 404 for a uuid that resolves to
//! nothing.

mod common;

use alexandria_core::config::Settings;
use alexandria_http::app;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use sqlx::sqlite::SqlitePool;
use tower::ServiceExt;

use crate::common::test_app;

fn authed_request(method: &str, uri: &str, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .header("content-type", "application/json");
    let body = match body {
        Some(v) => Body::from(v.to_string()),
        None => Body::empty(),
    };
    builder.body(body).unwrap()
}

fn unauthenticated_request(method: &str, uri: &str, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    let body = match body {
        Some(v) => Body::from(v.to_string()),
        None => Body::empty(),
    };
    builder.body(body).unwrap()
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

/// Insert a file of `file_type`, optionally tagged, and return its uuid.
/// Mirrors `playlists_api.rs`'s `seed_file`, with the tags the rankings
/// group by.
async fn seed_file(
    pool: &SqlitePool,
    file_type: &str,
    name: &str,
    tags: Option<(&str, &str, &str)>,
) -> String {
    let file_uuid = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO files (uuid, path, name, type, content_hash, indexed_at) \
         VALUES (?, ?, ?, ?, 'hash', ?)",
    )
    .bind(&file_uuid)
    .bind(format!("/lib/{name}"))
    .bind(name)
    .bind(file_type)
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(pool)
    .await
    .expect("seed file");

    if let Some((title, artist, album)) = tags {
        sqlx::query(
            "INSERT INTO audio_files (file_id, title, artist, album, genre) \
             VALUES ((SELECT id FROM files WHERE uuid = ?), ?, ?, ?, 'Jazz')",
        )
        .bind(&file_uuid)
        .bind(title)
        .bind(artist)
        .bind(album)
        .execute(pool)
        .await
        .expect("seed tags");
    }

    file_uuid
}

async fn play_count(pool: &SqlitePool) -> i64 {
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM play_events")
        .fetch_one(pool)
        .await
        .expect("count");
    count
}

#[tokio::test]
async fn given_an_audio_file_when_a_play_is_posted_then_it_is_created_and_recorded() {
    let app_state = test_app().await;
    let track = seed_file(
        &app_state.pool,
        "audio",
        "one.flac",
        Some(("Often", "Ada", "First")),
    )
    .await;

    let response = app(Settings::default(), app_state.services.clone())
        .oneshot(authed_request(
            "POST",
            "/v1/plays",
            Some(json!({ "fileUuid": track })),
        ))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    assert_eq!(body["fileUuid"], track);
    // The core stamped it: the body carries a timestamp the request never
    // sent.
    assert!(
        chrono::DateTime::parse_from_rfc3339(body["playedAt"].as_str().expect("playedAt")).is_ok(),
        "playedAt is an RFC 3339 instant"
    );
    assert_eq!(play_count(&app_state.pool).await, 1);
}

#[tokio::test]
async fn given_the_same_track_twice_when_posted_then_both_plays_are_recorded() {
    let app_state = test_app().await;
    let track = seed_file(&app_state.pool, "audio", "one.flac", None).await;

    for _ in 0..2 {
        let response = app(Settings::default(), app_state.services.clone())
            .oneshot(authed_request(
                "POST",
                "/v1/plays",
                Some(json!({ "fileUuid": track })),
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    // No idempotency of any kind: the second POST is a second play.
    assert_eq!(play_count(&app_state.pool).await, 2);
}

#[tokio::test]
async fn given_a_file_that_is_not_audio_when_a_play_is_posted_then_bad_request() {
    let app_state = test_app().await;
    let notes = seed_file(&app_state.pool, "text", "notes.txt", None).await;

    let response = app(Settings::default(), app_state.services.clone())
        .oneshot(authed_request(
            "POST",
            "/v1/plays",
            Some(json!({ "fileUuid": notes })),
        ))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(play_count(&app_state.pool).await, 0);
}

#[tokio::test]
async fn given_an_unknown_uuid_when_a_play_is_posted_then_not_found() {
    let app_state = test_app().await;

    let response = app(Settings::default(), app_state.services.clone())
        .oneshot(authed_request(
            "POST",
            "/v1/plays",
            Some(json!({ "fileUuid": uuid::Uuid::new_v4() })),
        ))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn given_a_body_that_is_not_a_uuid_when_a_play_is_posted_then_bad_request() {
    let app_state = test_app().await;

    let response = app(Settings::default(), app_state.services.clone())
        .oneshot(authed_request(
            "POST",
            "/v1/plays",
            Some(json!({ "fileUuid": "not-a-uuid" })),
        ))
        .await
        .expect("response");

    // This surface's envelope, not axum's bare-text 422.
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert!(body.get("error").is_some(), "carries the error envelope");
}

#[tokio::test]
async fn given_no_token_when_a_play_is_posted_then_unauthorized_before_the_body_is_read() {
    let app_state = test_app().await;

    let response = app(Settings::default(), app_state.services.clone())
        .oneshot(unauthenticated_request(
            "POST",
            "/v1/plays",
            Some(json!({ "fileUuid": "not-a-uuid" })),
        ))
        .await
        .expect("response");

    // The body is nonsense, and the answer is still 401: the gate runs
    // before the route's extractors (FR-AU-07).
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(play_count(&app_state.pool).await, 0);
}

#[tokio::test]
async fn given_recorded_plays_when_stats_are_read_then_the_body_carries_the_rankings() {
    let app_state = test_app().await;
    let often = seed_file(
        &app_state.pool,
        "audio",
        "often.flac",
        Some(("Often", "Ada", "First")),
    )
    .await;
    let seldom = seed_file(
        &app_state.pool,
        "audio",
        "seldom.flac",
        Some(("Seldom", "Bruno", "Second")),
    )
    .await;
    for (track, times) in [(&often, 3), (&seldom, 1)] {
        for _ in 0..times {
            app(Settings::default(), app_state.services.clone())
                .oneshot(authed_request(
                    "POST",
                    "/v1/plays",
                    Some(json!({ "fileUuid": track })),
                ))
                .await
                .expect("response");
        }
    }

    let response = app(Settings::default(), app_state.services.clone())
        .oneshot(authed_request("GET", "/v1/plays/stats", None))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["totalPlays"], 4);
    assert_eq!(body["distinctTracks"], 2);
    assert_eq!(body["topTracks"][0]["title"], "Often");
    assert_eq!(body["topTracks"][0]["plays"], 3);
    assert_eq!(body["topArtists"][0]["artist"], "Ada");
    assert_eq!(body["topAlbums"][0]["album"], "First");
    assert_eq!(body["topGenres"][0]["genre"], "Jazz");
}

#[tokio::test]
async fn given_a_limit_when_stats_are_read_then_the_rankings_are_cut_to_it() {
    let app_state = test_app().await;
    for n in 0..3 {
        let track = seed_file(
            &app_state.pool,
            "audio",
            &format!("track-{n}.flac"),
            Some(("Title", "Ada", "First")),
        )
        .await;
        app(Settings::default(), app_state.services.clone())
            .oneshot(authed_request(
                "POST",
                "/v1/plays",
                Some(json!({ "fileUuid": track })),
            ))
            .await
            .expect("response");
    }

    let response = app(Settings::default(), app_state.services.clone())
        .oneshot(authed_request("GET", "/v1/plays/stats?limit=2", None))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["topTracks"].as_array().expect("tracks").len(), 2);
    // The summary still counts everything: a chart cut to two is not the
    // owner having played two tracks.
    assert_eq!(body["totalPlays"], 3);
}

#[tokio::test]
async fn given_a_limit_outside_the_range_when_stats_are_read_then_bad_request() {
    let app_state = test_app().await;

    for uri in ["/v1/plays/stats?limit=0", "/v1/plays/stats?limit=101"] {
        let response = app(Settings::default(), app_state.services.clone())
            .oneshot(authed_request("GET", uri, None))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
    }
}

#[tokio::test]
async fn given_no_token_when_stats_are_read_then_unauthorized() {
    let app_state = test_app().await;

    let response = app(Settings::default(), app_state.services.clone())
        .oneshot(unauthenticated_request("GET", "/v1/plays/stats", None))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
