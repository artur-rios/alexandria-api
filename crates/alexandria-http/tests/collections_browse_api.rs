//! UC-46 integration tests for `GET /v1/collections` (Testing Specification
//! §7): the real axum router over a real temp SQLite database. Covers the main
//! flow with and without the `kind` filter, the derived item count, and every
//! alternative flow (AF-01 nothing to list, AF-02 unrecognised kind, AF-03
//! unauthorized).

mod common;

use alexandria_core::config::Settings;
use alexandria_http::app;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use sqlx::sqlite::SqlitePool;
use tower::ServiceExt;

use crate::common::test_app;

fn list_request(query: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(format!("/v1/collections{query}"))
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .body(Body::empty())
        .unwrap()
}

/// The same request with no `Authorization` header (AF-03).
fn unauthenticated_request() -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri("/v1/collections")
        .body(Body::empty())
        .unwrap()
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

/// Insert a collection directly and answer its internal id, so a test can
/// point rows at it without going through UC-10.
async fn seed_collection(pool: &SqlitePool, uuid: &str, name: &str, kind: &str) -> i64 {
    sqlx::query("INSERT INTO collections (uuid, name, kind) VALUES (?, ?, ?)")
        .bind(uuid)
        .bind(name)
        .bind(kind)
        .execute(pool)
        .await
        .expect("seed collection");

    sqlx::query_scalar("SELECT id FROM collections WHERE uuid = ?")
        .bind(uuid)
        .fetch_one(pool)
        .await
        .expect("collection id")
}

/// Insert a file row in `state`, optionally filed in `collection_id`.
async fn seed_file(pool: &SqlitePool, uuid: &str, collection_id: Option<i64>, state: &str) {
    sqlx::query(
        "INSERT INTO files (uuid, path, name, type, content_hash, state, indexed_at, collection_id) \
         VALUES (?, ?, ?, 'text', 'hash', ?, ?, ?)",
    )
    .bind(uuid)
    .bind(format!("/lib/{uuid}.txt"))
    .bind(format!("{uuid}.txt"))
    .bind(state)
    .bind(chrono::Utc::now().to_rfc3339())
    .bind(collection_id)
    .execute(pool)
    .await
    .expect("seed file");
}

/// Insert a bookmark row in `state`, optionally filed in `collection_id`.
async fn seed_bookmark(pool: &SqlitePool, uuid: &str, collection_id: Option<i64>, state: &str) {
    sqlx::query(
        "INSERT INTO bookmarks (uuid, url, title, state, collection_id) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(uuid)
    .bind(format!("https://example.org/{uuid}"))
    .bind("A page")
    .bind(state)
    .bind(collection_id)
    .execute(pool)
    .await
    .expect("seed bookmark");
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_collections_of_both_kinds_when_listed_then_200_with_all_of_them() {
    let test = test_app().await;
    seed_collection(
        &test.pool,
        "11111111-1111-4111-8111-111111111111",
        "Films",
        "file",
    )
    .await;
    seed_collection(
        &test.pool,
        "22222222-2222-4222-8222-222222222222",
        "Reading",
        "bookmark",
    )
    .await;
    let router = app(Settings::default(), test.services);

    let response = router.oneshot(list_request("")).await.expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let items = body.as_array().expect("array");
    assert_eq!(items.len(), 2);
    // Ordered by name, so the listing is stable between calls.
    assert_eq!(items[0]["name"], "Films");
    assert_eq!(items[0]["kind"], "file");
    assert_eq!(items[1]["name"], "Reading");
    assert_eq!(items[1]["kind"], "bookmark");
}

#[tokio::test]
async fn given_collections_of_both_kinds_when_listed_by_kind_then_only_that_kind_returned() {
    let test = test_app().await;
    seed_collection(
        &test.pool,
        "11111111-1111-4111-8111-111111111111",
        "Films",
        "file",
    )
    .await;
    seed_collection(
        &test.pool,
        "22222222-2222-4222-8222-222222222222",
        "Reading",
        "bookmark",
    )
    .await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(list_request("?kind=bookmark"))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let items = body.as_array().expect("array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["name"], "Reading");
    assert_eq!(items[0]["uuid"], "22222222-2222-4222-8222-222222222222");
}

#[tokio::test]
async fn given_a_file_collection_with_members_when_listed_then_item_count_matches() {
    let test = test_app().await;
    let id = seed_collection(
        &test.pool,
        "11111111-1111-4111-8111-111111111111",
        "Films",
        "file",
    )
    .await;
    seed_file(
        &test.pool,
        "aaaaaaaa-0000-4000-8000-000000000001",
        Some(id),
        "active",
    )
    .await;
    seed_file(
        &test.pool,
        "aaaaaaaa-0000-4000-8000-000000000002",
        Some(id),
        "active",
    )
    .await;
    // Filed nowhere: it must not be counted against the collection.
    seed_file(
        &test.pool,
        "aaaaaaaa-0000-4000-8000-000000000003",
        None,
        "active",
    )
    .await;
    let router = app(Settings::default(), test.services);

    let response = router.oneshot(list_request("")).await.expect("one-shot");

    let body = body_json(response).await;
    assert_eq!(body[0]["itemCount"], 2);
}

#[tokio::test]
async fn given_a_bookmark_collection_with_members_when_listed_then_item_count_matches() {
    let test = test_app().await;
    let id = seed_collection(
        &test.pool,
        "22222222-2222-4222-8222-222222222222",
        "Reading",
        "bookmark",
    )
    .await;
    seed_bookmark(
        &test.pool,
        "bbbbbbbb-0000-4000-8000-000000000001",
        Some(id),
        "active",
    )
    .await;
    let router = app(Settings::default(), test.services);

    let response = router.oneshot(list_request("")).await.expect("one-shot");

    let body = body_json(response).await;
    assert_eq!(body[0]["itemCount"], 1);
}

/// The count describes the same membership `GET /v1/collections/{uuid}/items`
/// lists, and that listing excludes soft-deleted records — so this one must
/// too, or the number would disagree with the list it summarises.
#[tokio::test]
async fn given_a_soft_deleted_member_when_listed_then_it_is_not_counted() {
    let test = test_app().await;
    let id = seed_collection(
        &test.pool,
        "11111111-1111-4111-8111-111111111111",
        "Films",
        "file",
    )
    .await;
    seed_file(
        &test.pool,
        "aaaaaaaa-0000-4000-8000-000000000001",
        Some(id),
        "active",
    )
    .await;
    seed_file(
        &test.pool,
        "aaaaaaaa-0000-4000-8000-000000000002",
        Some(id),
        "deleted",
    )
    .await;
    let router = app(Settings::default(), test.services);

    let response = router.oneshot(list_request("")).await.expect("one-shot");

    let body = body_json(response).await;
    assert_eq!(body[0]["itemCount"], 1);
}

// ---------------- Alternative flows ----------------

/// AF-01: nothing to list is an empty array and `200`, not an error.
#[tokio::test]
async fn given_no_collections_when_listed_then_200_with_empty_array() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router.oneshot(list_request("")).await.expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body.as_array().expect("array").len(), 0);
}

/// AF-01, reached the other way: collections exist, none of the kind asked
/// for.
#[tokio::test]
async fn given_no_collection_of_that_kind_when_listed_by_kind_then_200_with_empty_array() {
    let test = test_app().await;
    seed_collection(
        &test.pool,
        "11111111-1111-4111-8111-111111111111",
        "Films",
        "file",
    )
    .await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(list_request("?kind=bookmark"))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body.as_array().expect("array").len(), 0);
}

/// An empty `kind` is no filter at all, not an unrecognised one.
#[tokio::test]
async fn given_an_empty_kind_when_listed_then_treated_as_no_filter() {
    let test = test_app().await;
    seed_collection(
        &test.pool,
        "11111111-1111-4111-8111-111111111111",
        "Films",
        "file",
    )
    .await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(list_request("?kind="))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body.as_array().expect("array").len(), 1);
}

/// AF-02: an unrecognised kind is refused rather than silently listing
/// everything.
#[tokio::test]
async fn given_an_unknown_kind_when_listed_then_400_invalid_input() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(list_request("?kind=playlist"))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert!(
        body["error"]
            .as_str()
            .expect("error string")
            .contains("kind"),
        "the message names what was wrong: {body}"
    );
}

/// AF-03: the caller must be authenticated.
#[tokio::test]
async fn given_unauthenticated_when_listed_then_401() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(unauthenticated_request())
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
