//! UC-15 integration tests for `POST /v1/bookmarks` (Testing Specification
//! §7): the real axum router over a real temp SQLite database. Each test
//! asserts both the response and the resulting persisted state, and covers
//! the main flow plus every alternative flow (AF-01 invalid input, AF-02
//! referenced collection, AF-03 unauthorized).

mod common;

use alexandria_core::config::Settings;
use alexandria_http::app;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use sqlx::sqlite::SqlitePool;
use tower::ServiceExt;

use crate::common::test_app;

/// `(uuid, url, title, collection_id)` for every persisted bookmark, ordered
/// by title.
async fn bookmark_rows(pool: &SqlitePool) -> Vec<(String, String, String, Option<i64>)> {
    sqlx::query_as("SELECT uuid, url, title, collection_id FROM bookmarks ORDER BY title")
        .fetch_all(pool)
        .await
        .expect("rows")
}

fn create_request(body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/bookmarks")
        .header("authorization", "Bearer test-token")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn unauthenticated_request(body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/bookmarks")
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

async fn create_bookmark_collection(router: axum::Router, name: &str) -> (axum::Router, String) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/collections")
                .header("authorization", "Bearer test-token")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "name": name, "kind": "bookmark" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .expect("create collection");
    assert_eq!(response.status(), StatusCode::CREATED);
    let uuid = body_json(response).await["uuid"]
        .as_str()
        .unwrap()
        .to_string();
    (router, uuid)
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_valid_url_and_title_when_posted_then_201_with_bookmark_and_row_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(create_request(
            json!({ "url": "https://example.com/article", "title": "An article" }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    assert_eq!(body["url"], "https://example.com/article");
    assert_eq!(body["title"], "An article");
    assert!(body["collectionUuid"].is_null());
    let uuid = body["uuid"].as_str().expect("uuid string");
    assert!(uuid::Uuid::parse_str(uuid).is_ok(), "uuid is a valid UUID");

    let rows = bookmark_rows(&test.pool).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, uuid);
    assert_eq!(rows[0].1, "https://example.com/article");
    assert_eq!(rows[0].2, "An article");
    assert_eq!(rows[0].3, None);
}

#[tokio::test]
async fn given_bookmark_collection_when_posted_then_row_linked_to_collection() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, collection_uuid) = create_bookmark_collection(router, "Reading list").await;

    let response = router
        .oneshot(create_request(json!({
            "url": "https://example.com",
            "title": "Example",
            "collectionUuid": collection_uuid,
        })))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    assert_eq!(body["collectionUuid"], collection_uuid);

    let (collection_id,): (i64,) = sqlx::query_as("SELECT id FROM collections WHERE uuid = ?")
        .bind(&collection_uuid)
        .fetch_one(&test.pool)
        .await
        .unwrap();
    let rows = bookmark_rows(&test.pool).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].3, Some(collection_id));
}

// ---------------- AF-01: invalid input ----------------

#[tokio::test]
async fn given_empty_url_when_posted_then_400_and_nothing_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(create_request(json!({ "url": "", "title": "Title" })))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_json(response).await["error"].is_string());
    assert!(bookmark_rows(&test.pool).await.is_empty());
}

#[tokio::test]
async fn given_url_without_scheme_when_posted_then_400_and_nothing_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(create_request(
            json!({ "url": "example.com/no-scheme", "title": "Title" }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(bookmark_rows(&test.pool).await.is_empty());
}

#[tokio::test]
async fn given_empty_title_when_posted_then_400_and_nothing_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(create_request(
            json!({ "url": "https://example.com", "title": "" }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(bookmark_rows(&test.pool).await.is_empty());
}

#[tokio::test]
async fn given_missing_title_when_posted_then_400_and_nothing_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(create_request(json!({ "url": "https://example.com" })))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(bookmark_rows(&test.pool).await.is_empty());
}

#[tokio::test]
async fn given_malformed_json_body_when_posted_then_400_with_error_envelope() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let request = Request::builder()
        .method("POST")
        .uri("/v1/bookmarks")
        .header("authorization", "Bearer test-token")
        .header("content-type", "application/json")
        .body(Body::from("{ not json"))
        .unwrap();
    let response = router.oneshot(request).await.expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        body_json(response).await["error"].is_string(),
        "a parse failure uses this surface's error envelope, not axum's bare text"
    );
    assert!(bookmark_rows(&test.pool).await.is_empty());
}

// ---------------- AF-02: referenced collection ----------------

#[tokio::test]
async fn given_file_kind_collection_when_posted_then_400_and_nothing_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let create_collection_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/collections")
                .header("authorization", "Bearer test-token")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "name": "My files", "kind": "file" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .expect("create collection");
    let collection_uuid = body_json(create_collection_resp).await["uuid"]
        .as_str()
        .unwrap()
        .to_string();

    let response = router
        .oneshot(create_request(json!({
            "url": "https://example.com",
            "title": "Example",
            "collectionUuid": collection_uuid,
        })))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(bookmark_rows(&test.pool).await.is_empty());
}

#[tokio::test]
async fn given_unknown_collection_uuid_when_posted_then_404_and_nothing_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(create_request(json!({
            "url": "https://example.com",
            "title": "Example",
            "collectionUuid": uuid::Uuid::new_v4().to_string(),
        })))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(bookmark_rows(&test.pool).await.is_empty());
}

// ---------------- AF-03: unauthorized ----------------

#[tokio::test]
async fn given_no_token_when_posted_then_401_and_nothing_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(unauthenticated_request(
            json!({ "url": "https://example.com", "title": "Example" }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(bookmark_rows(&test.pool).await.is_empty());
}

#[tokio::test]
async fn given_no_token_and_malformed_body_when_posted_then_401_not_400() {
    // Authentication is evaluated before the body is parsed (FR-AU-07 / SRD
    // §7): an unauthenticated caller must not learn that its payload was also
    // unacceptable.
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let request = Request::builder()
        .method("POST")
        .uri("/v1/bookmarks")
        .header("content-type", "application/json")
        .body(Body::from("{ not json"))
        .unwrap();
    let response = router.oneshot(request).await.expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(bookmark_rows(&test.pool).await.is_empty());
}

// ==================== UC-16: Update a bookmark ====================

fn update_request(uuid: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("PATCH")
        .uri(format!("/v1/bookmarks/{uuid}"))
        .header("authorization", "Bearer test-token")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn unauthenticated_update_request(uuid: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("PATCH")
        .uri(format!("/v1/bookmarks/{uuid}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn create_bookmark(router: axum::Router, url: &str, title: &str) -> (axum::Router, String) {
    let response = router
        .clone()
        .oneshot(create_request(json!({ "url": url, "title": title })))
        .await
        .expect("create bookmark");
    assert_eq!(response.status(), StatusCode::CREATED);
    let uuid = body_json(response).await["uuid"]
        .as_str()
        .unwrap()
        .to_string();
    (router, uuid)
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_existing_bookmark_when_updated_then_200_with_updated_bookmark_and_row() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_bookmark(router, "https://example.com", "Example").await;

    let response = router
        .oneshot(update_request(
            &uuid,
            json!({ "url": "https://example.org", "title": "New title" }),
        ))
        .await
        .expect("update");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["url"], "https://example.org");
    assert_eq!(body["title"], "New title");

    let rows = bookmark_rows(&test.pool).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, "https://example.org");
    assert_eq!(rows[0].2, "New title");
}

#[tokio::test]
async fn given_bookmark_collection_when_updated_then_row_linked_to_collection() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_bookmark(router, "https://example.com", "Example").await;
    let (router, collection_uuid) = create_bookmark_collection(router, "Reading list").await;

    let response = router
        .oneshot(update_request(
            &uuid,
            json!({
                "url": "https://example.com",
                "title": "Example",
                "collectionUuid": collection_uuid,
            }),
        ))
        .await
        .expect("update");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["collectionUuid"], collection_uuid);
}

// ---------------- AF-01: invalid input ----------------

#[tokio::test]
async fn given_empty_url_when_updated_then_400_and_row_unchanged() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_bookmark(router, "https://example.com", "Example").await;

    let response = router
        .oneshot(update_request(
            &uuid,
            json!({ "url": "", "title": "Example" }),
        ))
        .await
        .expect("update");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(bookmark_rows(&test.pool).await[0].1, "https://example.com");
}

// ---------------- Referenced collection ----------------

#[tokio::test]
async fn given_file_kind_collection_when_updated_then_400_and_row_unchanged() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_bookmark(router, "https://example.com", "Example").await;
    let create_collection_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/collections")
                .header("authorization", "Bearer test-token")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "name": "My files", "kind": "file" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .expect("create collection");
    let collection_uuid = body_json(create_collection_resp).await["uuid"]
        .as_str()
        .unwrap()
        .to_string();

    let response = router
        .oneshot(update_request(
            &uuid,
            json!({
                "url": "https://example.com",
                "title": "Example",
                "collectionUuid": collection_uuid,
            }),
        ))
        .await
        .expect("update");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(bookmark_rows(&test.pool).await[0].3, None);
}

#[tokio::test]
async fn given_unknown_collection_uuid_when_updated_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_bookmark(router, "https://example.com", "Example").await;

    let response = router
        .oneshot(update_request(
            &uuid,
            json!({
                "url": "https://example.com",
                "title": "Example",
                "collectionUuid": uuid::Uuid::new_v4().to_string(),
            }),
        ))
        .await
        .expect("update");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------- AF-02: bookmark does not exist ----------------

#[tokio::test]
async fn given_unknown_bookmark_uuid_when_updated_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(update_request(
            &uuid::Uuid::new_v4().to_string(),
            json!({ "url": "https://example.com", "title": "Example" }),
        ))
        .await
        .expect("update");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn given_non_uuid_path_segment_when_updated_then_400() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(update_request(
            "not-a-uuid",
            json!({ "url": "https://example.com", "title": "Example" }),
        ))
        .await
        .expect("update");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ---------------- Precondition: bookmark must be active ----------------

#[tokio::test]
async fn given_soft_deleted_bookmark_when_updated_then_409() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_bookmark(router, "https://example.com", "Example").await;
    sqlx::query("UPDATE bookmarks SET state = 'deleted' WHERE uuid = ?")
        .bind(&uuid)
        .execute(&test.pool)
        .await
        .expect("mark deleted");

    let response = router
        .oneshot(update_request(
            &uuid,
            json!({ "url": "https://example.org", "title": "New title" }),
        ))
        .await
        .expect("update");

    assert_eq!(response.status(), StatusCode::CONFLICT);
}

// ---------------- AF-03: unauthorized ----------------

#[tokio::test]
async fn given_no_token_when_updated_then_401_and_row_unchanged() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_bookmark(router, "https://example.com", "Example").await;

    let response = router
        .oneshot(unauthenticated_update_request(
            &uuid,
            json!({ "url": "https://example.org", "title": "New title" }),
        ))
        .await
        .expect("update");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(bookmark_rows(&test.pool).await[0].1, "https://example.com");
}
