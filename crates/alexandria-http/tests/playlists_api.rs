//! HTTP integration tests for the playlists surface (Task 8, `/v1/playlists`
//! and its sub-routes): the real axum router over a real temp SQLite
//! database. One test per route, plus the status mappings that are easy to
//! get wrong -- 401 before the body is parsed, 400 for a blank name and for
//! an out-of-range move index, and 404 for an unknown playlist.

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

/// `(uuid, name)` for every persisted playlist, ordered by name.
async fn playlist_rows(pool: &SqlitePool) -> Vec<(String, String)> {
    sqlx::query_as("SELECT uuid, name FROM playlists ORDER BY name")
        .fetch_all(pool)
        .await
        .expect("rows")
}

/// Insert a minimal `files` row of the given `file_type` and return its
/// uuid, mirroring `watchlists_add_video_api.rs`'s `seed_file`.
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

async fn create_playlist(router: axum::Router, name: &str) -> (axum::Router, String) {
    let response = router
        .clone()
        .oneshot(authed_request(
            "POST",
            "/v1/playlists",
            Some(json!({ "name": name })),
        ))
        .await
        .expect("create playlist");
    assert_eq!(response.status(), StatusCode::CREATED);
    let uuid = body_json(response).await["uuid"]
        .as_str()
        .expect("uuid")
        .to_string();
    (router, uuid)
}

// ---------------- POST /v1/playlists ----------------

#[tokio::test]
async fn given_no_bearer_when_a_playlist_is_created_then_401() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(unauthenticated_request(
            "POST",
            "/v1/playlists",
            Some(json!({ "name": "Road trip" })),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(playlist_rows(&test.pool).await.is_empty());
}

#[tokio::test]
async fn given_no_bearer_and_malformed_body_when_a_playlist_is_created_then_401() {
    // Authentication is evaluated before the body is parsed (FR-AU-07 / SRD
    // §7): an unauthenticated caller must not learn that its payload was
    // also unacceptable. Mirrors `reading_lists_api.rs`'s
    // `given_no_token_and_malformed_body_when_posted_then_401_not_400` --
    // with a well-formed body a no-bearer request would answer 401 whether
    // the route sits inside `require_auth`'s `route_layer` or not, since
    // the core handler's own `authenticate()` also answers 401. Only a
    // syntactically invalid body separates the two: a route registered
    // outside the layer would hit `create`'s `body.map_err(...)?` before
    // the core handler is ever reached and answer 400 instead.
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let request = Request::builder()
        .method("POST")
        .uri("/v1/playlists")
        .header("content-type", "application/json")
        .body(Body::from("{ not json"))
        .unwrap();
    let response = router.oneshot(request).await.expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(playlist_rows(&test.pool).await.is_empty());
}

#[tokio::test]
async fn given_a_blank_name_when_a_playlist_is_created_then_400() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(authed_request(
            "POST",
            "/v1/playlists",
            Some(json!({ "name": "   " })),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_json(response).await["error"].is_string());
    assert!(playlist_rows(&test.pool).await.is_empty());
}

#[tokio::test]
async fn given_a_valid_name_when_a_playlist_is_created_then_201_with_row_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let response = router
        .oneshot(authed_request(
            "POST",
            "/v1/playlists",
            Some(json!({ "name": "Road trip" })),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    assert_eq!(body["name"], "Road trip");
    let uuid = body["uuid"].as_str().expect("uuid string");
    assert!(uuid::Uuid::parse_str(uuid).is_ok(), "uuid is a valid UUID");

    let rows = playlist_rows(&test.pool).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, uuid);
    assert_eq!(rows[0].1, "Road trip");
}

// ---------------- PATCH /v1/playlists/{uuid} ----------------

#[tokio::test]
async fn given_no_bearer_when_a_playlist_is_renamed_then_401() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_playlist(router, "Road trip").await;

    let response = router
        .oneshot(unauthenticated_request(
            "PATCH",
            &format!("/v1/playlists/{uuid}"),
            Some(json!({ "name": "Summer trip" })),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(playlist_rows(&test.pool).await[0].1, "Road trip");
}

#[tokio::test]
async fn given_a_valid_name_when_a_playlist_is_renamed_then_200_with_row_updated() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_playlist(router, "Road trip").await;

    let response = router
        .oneshot(authed_request(
            "PATCH",
            &format!("/v1/playlists/{uuid}"),
            Some(json!({ "name": "Summer trip" })),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["uuid"], uuid);
    assert_eq!(body["name"], "Summer trip");
    assert_eq!(playlist_rows(&test.pool).await[0].1, "Summer trip");
}

#[tokio::test]
async fn given_an_unknown_playlist_when_renamed_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    let unknown = uuid::Uuid::new_v4().to_string();

    let response = router
        .oneshot(authed_request(
            "PATCH",
            &format!("/v1/playlists/{unknown}"),
            Some(json!({ "name": "Summer trip" })),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------- DELETE /v1/playlists/{uuid} ----------------

#[tokio::test]
async fn given_no_bearer_when_a_playlist_is_deleted_then_401() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_playlist(router, "Road trip").await;

    let response = router
        .oneshot(unauthenticated_request(
            "DELETE",
            &format!("/v1/playlists/{uuid}"),
            None,
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(playlist_rows(&test.pool).await.len(), 1);
}

#[tokio::test]
async fn given_a_known_playlist_when_deleted_then_200_and_row_removed() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_playlist(router, "Road trip").await;

    let response = router
        .oneshot(authed_request(
            "DELETE",
            &format!("/v1/playlists/{uuid}"),
            None,
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["uuid"], uuid);
    assert!(playlist_rows(&test.pool).await.is_empty());
}

#[tokio::test]
async fn given_an_unknown_playlist_when_deleted_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    let unknown = uuid::Uuid::new_v4().to_string();

    let response = router
        .oneshot(authed_request(
            "DELETE",
            &format!("/v1/playlists/{unknown}"),
            None,
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------- GET /v1/playlists ----------------

#[tokio::test]
async fn given_no_bearer_when_playlists_are_listed_then_401() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(unauthenticated_request("GET", "/v1/playlists", None))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn given_two_playlists_when_listed_then_200_with_both() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, _) = create_playlist(router, "Road trip").await;
    let (router, _) = create_playlist(router, "Workout").await;

    let response = router
        .oneshot(authed_request("GET", "/v1/playlists", None))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let names: Vec<&str> = body
        .as_array()
        .expect("array")
        .iter()
        .map(|p| p["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, vec!["Road trip", "Workout"]);
}

// ---------------- GET /v1/playlists/{uuid} ----------------

#[tokio::test]
async fn given_no_bearer_when_a_playlist_is_read_then_401() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_playlist(router, "Road trip").await;

    let response = router
        .oneshot(unauthenticated_request(
            "GET",
            &format!("/v1/playlists/{uuid}"),
            None,
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn given_an_unknown_playlist_when_read_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    let unknown = uuid::Uuid::new_v4().to_string();

    let response = router
        .oneshot(authed_request(
            "GET",
            &format!("/v1/playlists/{unknown}"),
            None,
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn given_a_playlist_when_read_then_the_body_carries_its_tracks_in_order() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_playlist(router, "Road trip").await;

    let first = seed_file(&test.pool, "audio").await;
    let second = seed_file(&test.pool, "audio").await;

    let add_response = router
        .clone()
        .oneshot(authed_request(
            "POST",
            &format!("/v1/playlists/{uuid}/entries"),
            Some(json!({ "fileUuids": [first, second] })),
        ))
        .await
        .expect("add entries");
    assert_eq!(add_response.status(), StatusCode::OK);

    let response = router
        .oneshot(authed_request(
            "GET",
            &format!("/v1/playlists/{uuid}"),
            None,
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["playlist"]["uuid"], uuid);
    let entries = body["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["position"], 0);
    assert_eq!(entries[0]["file"]["file"]["uuid"], first);
    assert_eq!(entries[0]["missing"], false);
    assert_eq!(entries[1]["position"], 1);
    assert_eq!(entries[1]["file"]["file"]["uuid"], second);
    assert_eq!(entries[1]["missing"], false);
}

// ---------------- POST /v1/playlists/{uuid}/entries ----------------

#[tokio::test]
async fn given_no_bearer_when_entries_are_added_then_401() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_playlist(router, "Road trip").await;
    let song = seed_file(&test.pool, "audio").await;

    let response = router
        .oneshot(unauthenticated_request(
            "POST",
            &format!("/v1/playlists/{uuid}/entries"),
            Some(json!({ "fileUuids": [song] })),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn given_valid_file_uuids_when_entries_are_added_then_200_with_positions_from_zero() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_playlist(router, "Road trip").await;
    let first = seed_file(&test.pool, "audio").await;
    let second = seed_file(&test.pool, "audio").await;

    let response = router
        .oneshot(authed_request(
            "POST",
            &format!("/v1/playlists/{uuid}/entries"),
            Some(json!({ "fileUuids": [first, second] })),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let entries = body.as_array().expect("array");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["position"], 0);
    assert_eq!(entries[1]["position"], 1);
}

#[tokio::test]
async fn given_a_non_audio_file_when_added_then_400() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_playlist(router, "Road trip").await;
    let video = seed_file(&test.pool, "video").await;

    let response = router
        .oneshot(authed_request(
            "POST",
            &format!("/v1/playlists/{uuid}/entries"),
            Some(json!({ "fileUuids": [video] })),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_json(response).await["error"].is_string());
}

#[tokio::test]
async fn given_an_unknown_playlist_when_entries_are_added_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let song = seed_file(&test.pool, "audio").await;
    let unknown = uuid::Uuid::new_v4().to_string();

    let response = router
        .oneshot(authed_request(
            "POST",
            &format!("/v1/playlists/{unknown}/entries"),
            Some(json!({ "fileUuids": [song] })),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------- DELETE /v1/playlists/{uuid}/entries/{entryUuid} ----------------

#[tokio::test]
async fn given_no_bearer_when_an_entry_is_removed_then_401() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_playlist(router, "Road trip").await;
    let song = seed_file(&test.pool, "audio").await;

    let add_response = router
        .clone()
        .oneshot(authed_request(
            "POST",
            &format!("/v1/playlists/{uuid}/entries"),
            Some(json!({ "fileUuids": [song] })),
        ))
        .await
        .expect("add entries");
    let entry_id = body_json(add_response).await[0]["uuid"]
        .as_str()
        .expect("entry uuid")
        .to_string();

    let response = router
        .oneshot(unauthenticated_request(
            "DELETE",
            &format!("/v1/playlists/{uuid}/entries/{entry_id}"),
            None,
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn given_a_known_entry_when_removed_then_200_and_row_removed() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_playlist(router, "Road trip").await;
    let song = seed_file(&test.pool, "audio").await;

    let add_response = router
        .clone()
        .oneshot(authed_request(
            "POST",
            &format!("/v1/playlists/{uuid}/entries"),
            Some(json!({ "fileUuids": [song] })),
        ))
        .await
        .expect("add entries");
    let entry_id = body_json(add_response).await[0]["uuid"]
        .as_str()
        .expect("entry uuid")
        .to_string();

    let response = router
        .clone()
        .oneshot(authed_request(
            "DELETE",
            &format!("/v1/playlists/{uuid}/entries/{entry_id}"),
            None,
        ))
        .await
        .expect("one-shot");
    assert_eq!(response.status(), StatusCode::OK);

    let read_response = router
        .oneshot(authed_request(
            "GET",
            &format!("/v1/playlists/{uuid}"),
            None,
        ))
        .await
        .expect("read");
    let body = body_json(read_response).await;
    assert!(body["entries"].as_array().expect("entries").is_empty());
}

#[tokio::test]
async fn given_an_unknown_entry_when_removed_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_playlist(router, "Road trip").await;

    let response = router
        .oneshot(authed_request(
            "DELETE",
            &format!("/v1/playlists/{uuid}/entries/{}", uuid::Uuid::new_v4()),
            None,
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------- POST /v1/playlists/{uuid}/entries/{entryUuid}/move ----------------

#[tokio::test]
async fn given_no_bearer_when_an_entry_is_moved_then_401() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_playlist(router, "Road trip").await;
    let first = seed_file(&test.pool, "audio").await;
    let second = seed_file(&test.pool, "audio").await;

    let add_response = router
        .clone()
        .oneshot(authed_request(
            "POST",
            &format!("/v1/playlists/{uuid}/entries"),
            Some(json!({ "fileUuids": [first, second] })),
        ))
        .await
        .expect("add entries");
    let entry_id = body_json(add_response).await[0]["uuid"]
        .as_str()
        .expect("entry uuid")
        .to_string();

    let response = router
        .oneshot(unauthenticated_request(
            "POST",
            &format!("/v1/playlists/{uuid}/entries/{entry_id}/move"),
            Some(json!({ "toIndex": 1 })),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn given_a_valid_index_when_an_entry_is_moved_then_200_with_new_order() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_playlist(router, "Road trip").await;
    let first = seed_file(&test.pool, "audio").await;
    let second = seed_file(&test.pool, "audio").await;

    let add_response = router
        .clone()
        .oneshot(authed_request(
            "POST",
            &format!("/v1/playlists/{uuid}/entries"),
            Some(json!({ "fileUuids": [first, second] })),
        ))
        .await
        .expect("add entries");
    let entries = body_json(add_response).await;
    let first_entry_id = entries[0]["uuid"].as_str().expect("entry uuid").to_string();

    let response = router
        .oneshot(authed_request(
            "POST",
            &format!("/v1/playlists/{uuid}/entries/{first_entry_id}/move"),
            Some(json!({ "toIndex": 1 })),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let order = body.as_array().expect("array");
    assert_eq!(order.len(), 2);
    assert_eq!(order[1]["uuid"], first_entry_id);
    assert_eq!(order[1]["position"], 1);
    assert_eq!(order[0]["position"], 0);
}

#[tokio::test]
async fn given_an_index_past_the_end_when_an_entry_is_moved_then_400() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_playlist(router, "Road trip").await;
    let song = seed_file(&test.pool, "audio").await;

    let add_response = router
        .clone()
        .oneshot(authed_request(
            "POST",
            &format!("/v1/playlists/{uuid}/entries"),
            Some(json!({ "fileUuids": [song] })),
        ))
        .await
        .expect("add entries");
    let entry_id = body_json(add_response).await[0]["uuid"]
        .as_str()
        .expect("entry uuid")
        .to_string();

    let response = router
        .oneshot(authed_request(
            "POST",
            &format!("/v1/playlists/{uuid}/entries/{entry_id}/move"),
            Some(json!({ "toIndex": 5 })),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_json(response).await["error"].is_string());
}

#[tokio::test]
async fn given_an_unknown_entry_when_moved_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_playlist(router, "Road trip").await;

    let response = router
        .oneshot(authed_request(
            "POST",
            &format!("/v1/playlists/{uuid}/entries/{}/move", uuid::Uuid::new_v4()),
            Some(json!({ "toIndex": 0 })),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
