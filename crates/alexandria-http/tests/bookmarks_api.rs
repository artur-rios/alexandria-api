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

// ==================== UC-17: Browse bookmarks ====================

fn list_request(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", "Bearer test-token")
        .body(Body::empty())
        .unwrap()
}

fn list_request_no_auth(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_bookmarks_when_listed_default_then_200_array_excluding_deleted_by_default() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, deleted_uuid) = create_bookmark(router, "https://example.com/a", "A").await;
    let (router, _kept_uuid) = create_bookmark(router, "https://example.com/b", "B").await;
    sqlx::query("UPDATE bookmarks SET state = 'deleted' WHERE uuid = ?")
        .bind(&deleted_uuid)
        .execute(&test.pool)
        .await
        .expect("mark deleted");

    let response = router
        .oneshot(list_request("/v1/bookmarks"))
        .await
        .expect("list");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let arr = body.as_array().expect("array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["title"], "B");
}

#[tokio::test]
async fn given_bookmarks_when_listed_filtered_by_collection_then_only_linked_returned() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, collection_uuid) = create_bookmark_collection(router, "Reading list").await;
    let (router, _linked_uuid) = {
        let response = router
            .clone()
            .oneshot(create_request(json!({
                "url": "https://example.com/a",
                "title": "A",
                "collectionUuid": collection_uuid,
            })))
            .await
            .expect("create linked");
        assert_eq!(response.status(), StatusCode::CREATED);
        let uuid = body_json(response).await["uuid"]
            .as_str()
            .unwrap()
            .to_string();
        (router, uuid)
    };
    let (router, _unlinked) = create_bookmark(router, "https://example.com/b", "B").await;

    let response = router
        .oneshot(list_request(&format!(
            "/v1/bookmarks?collectionUuid={collection_uuid}"
        )))
        .await
        .expect("list");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let arr = body.as_array().expect("array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["title"], "A");
}

#[tokio::test]
async fn given_deleted_bookmark_when_listed_state_all_then_included() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_bookmark(router, "https://example.com", "Example").await;
    sqlx::query("UPDATE bookmarks SET state = 'deleted' WHERE uuid = ?")
        .bind(&uuid)
        .execute(&test.pool)
        .await
        .expect("mark deleted");

    let response = router
        .oneshot(list_request("/v1/bookmarks?state=all"))
        .await
        .expect("list");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await.as_array().unwrap().len(), 1);
}

// ---------------- AF-01: referenced collection does not exist ----------------

#[tokio::test]
async fn given_unknown_collection_uuid_when_listed_filtered_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(list_request(&format!(
            "/v1/bookmarks?collectionUuid={}",
            uuid::Uuid::new_v4()
        )))
        .await
        .expect("list");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn given_malformed_collection_uuid_when_listed_filtered_then_400() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(list_request("/v1/bookmarks?collectionUuid=not-a-uuid"))
        .await
        .expect("list");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn given_unknown_state_when_listed_then_400() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(list_request("/v1/bookmarks?state=nonsense"))
        .await
        .expect("list");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ---------------- AF-02: unauthorized ----------------

#[tokio::test]
async fn given_no_token_when_listed_then_401() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(list_request_no_auth("/v1/bookmarks"))
        .await
        .expect("list");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ==================== UC-18: Soft-delete and restore a bookmark ====================

fn soft_delete_request(uuid: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(format!("/v1/bookmarks/{uuid}"))
        .header("authorization", "Bearer test-token")
        .body(Body::empty())
        .unwrap()
}

fn unauthenticated_soft_delete_request(uuid: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(format!("/v1/bookmarks/{uuid}"))
        .body(Body::empty())
        .unwrap()
}

fn restore_request(uuid: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/v1/bookmarks/{uuid}/restore"))
        .header("authorization", "Bearer test-token")
        .body(Body::empty())
        .unwrap()
}

fn unauthenticated_restore_request(uuid: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/v1/bookmarks/{uuid}/restore"))
        .body(Body::empty())
        .unwrap()
}

// ---------------- Main flow: soft-delete ----------------

#[tokio::test]
async fn given_active_bookmark_when_soft_deleted_then_200_and_state_deleted_in_row() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_bookmark(router, "https://example.com", "Example").await;

    let response = router
        .oneshot(soft_delete_request(&uuid))
        .await
        .expect("soft delete");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["state"], "deleted");
    assert!(!body["deletedAt"].is_null());

    let (state,): (String,) = sqlx::query_as("SELECT state FROM bookmarks WHERE uuid = ?")
        .bind(&uuid)
        .fetch_one(&test.pool)
        .await
        .unwrap();
    assert_eq!(state, "deleted");
}

#[tokio::test]
async fn given_deleted_bookmark_when_soft_deleted_again_then_409() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    let (router, uuid) = create_bookmark(router, "https://example.com", "Example").await;
    let router = {
        let resp = router
            .clone()
            .oneshot(soft_delete_request(&uuid))
            .await
            .expect("first delete");
        assert_eq!(resp.status(), StatusCode::OK);
        router
    };

    let response = router
        .oneshot(soft_delete_request(&uuid))
        .await
        .expect("second delete");

    assert_eq!(response.status(), StatusCode::CONFLICT);
}

// ---------------- AF-01 (soft-delete): not found ----------------

#[tokio::test]
async fn given_unknown_uuid_when_soft_deleted_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(soft_delete_request(&uuid::Uuid::new_v4().to_string()))
        .await
        .expect("soft delete");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------- AF-02 (soft-delete): unauthorized ----------------

#[tokio::test]
async fn given_no_token_when_soft_deleted_then_401() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_bookmark(router, "https://example.com", "Example").await;

    let response = router
        .oneshot(unauthenticated_soft_delete_request(&uuid))
        .await
        .expect("soft delete");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let (state,): (String,) = sqlx::query_as("SELECT state FROM bookmarks WHERE uuid = ?")
        .bind(&uuid)
        .fetch_one(&test.pool)
        .await
        .unwrap();
    assert_eq!(state, "active");
}

// ---------------- Main flow: restore ----------------

#[tokio::test]
async fn given_deleted_bookmark_when_restored_then_200_and_state_active_in_row() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_bookmark(router, "https://example.com", "Example").await;
    let resp = router
        .clone()
        .oneshot(soft_delete_request(&uuid))
        .await
        .expect("delete first");
    assert_eq!(resp.status(), StatusCode::OK);

    let response = router
        .oneshot(restore_request(&uuid))
        .await
        .expect("restore");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["state"], "active");
    assert!(body["deletedAt"].is_null());

    let (state,): (String,) = sqlx::query_as("SELECT state FROM bookmarks WHERE uuid = ?")
        .bind(&uuid)
        .fetch_one(&test.pool)
        .await
        .unwrap();
    assert_eq!(state, "active");
}

#[tokio::test]
async fn given_active_bookmark_when_restored_then_409() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    let (router, uuid) = create_bookmark(router, "https://example.com", "Example").await;

    let response = router
        .oneshot(restore_request(&uuid))
        .await
        .expect("restore");

    assert_eq!(response.status(), StatusCode::CONFLICT);
}

// ---------------- AF-01 (restore): not found ----------------

#[tokio::test]
async fn given_unknown_uuid_when_restored_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(restore_request(&uuid::Uuid::new_v4().to_string()))
        .await
        .expect("restore");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------- AF-02 (restore): unauthorized ----------------

#[tokio::test]
async fn given_no_token_when_restored_then_401() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_bookmark(router, "https://example.com", "Example").await;
    let resp = router
        .clone()
        .oneshot(soft_delete_request(&uuid))
        .await
        .expect("delete first");
    assert_eq!(resp.status(), StatusCode::OK);

    let response = router
        .oneshot(unauthenticated_restore_request(&uuid))
        .await
        .expect("restore");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let (state,): (String,) = sqlx::query_as("SELECT state FROM bookmarks WHERE uuid = ?")
        .bind(&uuid)
        .fetch_one(&test.pool)
        .await
        .unwrap();
    assert_eq!(state, "deleted");
}

// ==================== UC-19: Hard-purge a bookmark ====================

fn purge_request(uuid: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(format!("/v1/bookmarks/{uuid}?purge=true"))
        .header("authorization", "Bearer test-token")
        .body(Body::empty())
        .unwrap()
}

/// Mark `uuid` deleted with a `deleted_at` well past the default 30-day
/// retention window.
async fn mark_deleted_past_retention(pool: &sqlx::sqlite::SqlitePool, uuid: &str) {
    sqlx::query("UPDATE bookmarks SET state = 'deleted', deleted_at = ? WHERE uuid = ?")
        .bind("2024-01-01T00:00:00Z")
        .bind(uuid)
        .execute(pool)
        .await
        .expect("past-retention seed");
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_deleted_bookmark_past_retention_when_purged_then_200_and_row_removed() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_bookmark(router, "https://example.com", "Example").await;
    mark_deleted_past_retention(&test.pool, &uuid).await;

    let response = router.oneshot(purge_request(&uuid)).await.expect("purge");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["uuid"], uuid);
    assert_eq!(
        body["state"], "deleted",
        "confirmation echoes pre-purge state"
    );

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bookmarks WHERE uuid = ?")
        .bind(&uuid)
        .fetch_one(&test.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

// ---------------- AF-01: retention window not elapsed ----------------

#[tokio::test]
async fn given_active_bookmark_when_purged_then_409_and_row_kept() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_bookmark(router, "https://example.com", "Example").await;

    let response = router.oneshot(purge_request(&uuid)).await.expect("purge");

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bookmarks WHERE uuid = ?")
        .bind(&uuid)
        .fetch_one(&test.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn given_deleted_bookmark_within_retention_when_purged_then_409_and_row_kept() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_bookmark(router, "https://example.com", "Example").await;
    let resp = router
        .clone()
        .oneshot(soft_delete_request(&uuid))
        .await
        .expect("delete");
    assert_eq!(resp.status(), StatusCode::OK);

    let response = router.oneshot(purge_request(&uuid)).await.expect("purge");

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bookmarks WHERE uuid = ?")
        .bind(&uuid)
        .fetch_one(&test.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

// ---------------- AF-02: bookmark does not exist ----------------

#[tokio::test]
async fn given_unknown_uuid_when_purged_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(purge_request(&uuid::Uuid::new_v4().to_string()))
        .await
        .expect("purge");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------- AF-03: unauthorized ----------------

#[tokio::test]
async fn given_no_token_when_purged_then_401_and_row_kept() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, uuid) = create_bookmark(router, "https://example.com", "Example").await;
    mark_deleted_past_retention(&test.pool, &uuid).await;

    let request = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/bookmarks/{uuid}?purge=true"))
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.expect("purge");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bookmarks WHERE uuid = ?")
        .bind(&uuid)
        .fetch_one(&test.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}
