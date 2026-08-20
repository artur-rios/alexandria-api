//! UC-10 integration tests for `POST /v1/collections` (Testing Specification
//! §7): the real axum router over a real temp SQLite database. Each test
//! asserts both the response and the resulting persisted state, and covers the
//! main flow plus every alternative flow (AF-01 invalid input, AF-02
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

/// `(uuid, name, kind)` for every persisted collection, ordered by name.
async fn collection_rows(pool: &SqlitePool) -> Vec<(String, String, String)> {
    sqlx::query_as("SELECT uuid, name, kind FROM collections ORDER BY name")
        .fetch_all(pool)
        .await
        .expect("rows")
}

fn create_request(body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/collections")
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// The same request with no `Authorization` header (AF-02).
fn unauthenticated_request(body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/collections")
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

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_valid_file_collection_when_posted_then_201_with_collection_and_row_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(create_request(
            json!({ "name": "Sci-fi novels", "kind": "file" }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    assert_eq!(body["name"], "Sci-fi novels");
    assert_eq!(body["kind"], "file");
    let uuid = body["uuid"].as_str().expect("uuid string");
    assert!(uuid::Uuid::parse_str(uuid).is_ok(), "uuid is a valid UUID");

    let rows = collection_rows(&test.pool).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, uuid, "the returned uuid is the persisted one");
    assert_eq!(rows[0].1, "Sci-fi novels");
    assert_eq!(rows[0].2, "file");
}

#[tokio::test]
async fn given_valid_bookmark_collection_when_posted_then_kind_persisted_as_bookmark() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(create_request(
            json!({ "name": "Rust reading", "kind": "bookmark" }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(body_json(response).await["kind"], "bookmark");

    let rows = collection_rows(&test.pool).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].2, "bookmark");
}

#[tokio::test]
async fn given_same_name_twice_when_posted_then_both_created_with_distinct_uuids() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let first = router
        .clone()
        .oneshot(create_request(
            json!({ "name": "Favorites", "kind": "file" }),
        ))
        .await
        .expect("first");
    assert_eq!(first.status(), StatusCode::CREATED);
    let second = router
        .oneshot(create_request(
            json!({ "name": "Favorites", "kind": "file" }),
        ))
        .await
        .expect("second");
    assert_eq!(second.status(), StatusCode::CREATED);

    let first_uuid = body_json(first).await["uuid"].as_str().unwrap().to_string();
    let second_uuid = body_json(second).await["uuid"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(first_uuid, second_uuid);
    assert_eq!(collection_rows(&test.pool).await.len(), 2);
}

// ---------------- AF-01: invalid input ----------------

#[tokio::test]
async fn given_empty_name_when_posted_then_400_and_nothing_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(create_request(json!({ "name": "", "kind": "file" })))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_json(response).await["error"].is_string());
    assert!(collection_rows(&test.pool).await.is_empty());
}

#[tokio::test]
async fn given_blank_name_when_posted_then_400_and_nothing_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(create_request(json!({ "name": "   ", "kind": "file" })))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(collection_rows(&test.pool).await.is_empty());
}

#[tokio::test]
async fn given_unrecognised_kind_when_posted_then_400_and_nothing_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(create_request(
            json!({ "name": "Mixed bag", "kind": "playlist" }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_json(response).await["error"].is_string());
    assert!(collection_rows(&test.pool).await.is_empty());
}

#[tokio::test]
async fn given_missing_kind_when_posted_then_400_and_nothing_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(create_request(json!({ "name": "Sci-fi novels" })))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(collection_rows(&test.pool).await.is_empty());
}

#[tokio::test]
async fn given_malformed_json_body_when_posted_then_400_with_error_envelope() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let request = Request::builder()
        .method("POST")
        .uri("/v1/collections")
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .header("content-type", "application/json")
        .body(Body::from("{ not json"))
        .unwrap();
    let response = router.oneshot(request).await.expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        body_json(response).await["error"].is_string(),
        "a parse failure uses this surface's error envelope, not axum's bare text"
    );
    assert!(collection_rows(&test.pool).await.is_empty());
}

// ---------------- AF-02: unauthorized ----------------

#[tokio::test]
async fn given_no_token_when_posted_then_401_and_nothing_persisted() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(unauthenticated_request(
            json!({ "name": "Sci-fi novels", "kind": "file" }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(collection_rows(&test.pool).await.is_empty());
}

// ==================== UC-11: Rename a collection ====================

fn rename_request(uuid: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("PATCH")
        .uri(format!("/v1/collections/{uuid}"))
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn unauthenticated_rename_request(uuid: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("PATCH")
        .uri(format!("/v1/collections/{uuid}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Create a collection via the router and return its uuid.
async fn create_collection(router: axum::Router, name: &str) -> (axum::Router, String) {
    let response = router
        .clone()
        .oneshot(create_request(json!({ "name": name, "kind": "file" })))
        .await
        .expect("create");
    assert_eq!(response.status(), StatusCode::CREATED);
    let uuid = body_json(response).await["uuid"]
        .as_str()
        .unwrap()
        .to_string();
    (router, uuid)
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_existing_collection_when_renamed_then_200_with_updated_collection_and_row() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    let (router, uuid) = create_collection(router, "Sci-fi novels").await;

    let response = router
        .oneshot(rename_request(&uuid, json!({ "name": "Sci-fi & fantasy" })))
        .await
        .expect("rename");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["uuid"], uuid);
    assert_eq!(body["name"], "Sci-fi & fantasy");

    let rows = collection_rows(&test.pool).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, "Sci-fi & fantasy");
}

// ---------------- AF-01: invalid input ----------------

#[tokio::test]
async fn given_empty_name_when_renamed_then_400_and_name_unchanged() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    let (router, uuid) = create_collection(router, "Sci-fi novels").await;

    let response = router
        .oneshot(rename_request(&uuid, json!({ "name": "" })))
        .await
        .expect("rename");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_json(response).await["error"].is_string());
    assert_eq!(collection_rows(&test.pool).await[0].1, "Sci-fi novels");
}

// ---------------- AF-02: not found ----------------

#[tokio::test]
async fn given_unknown_uuid_when_renamed_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(rename_request(
            &uuid::Uuid::new_v4().to_string(),
            json!({ "name": "New name" }),
        ))
        .await
        .expect("rename");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn given_non_uuid_path_segment_when_renamed_then_400() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(rename_request("not-a-uuid", json!({ "name": "New name" })))
        .await
        .expect("rename");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ---------------- AF-03: unauthorized ----------------

#[tokio::test]
async fn given_no_token_when_renamed_then_401_and_name_unchanged() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    let (router, uuid) = create_collection(router, "Sci-fi novels").await;

    let response = router
        .oneshot(unauthenticated_rename_request(
            &uuid,
            json!({ "name": "New name" }),
        ))
        .await
        .expect("rename");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(collection_rows(&test.pool).await[0].1, "Sci-fi novels");
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
        .uri("/v1/collections")
        .header("content-type", "application/json")
        .body(Body::from("{ not json"))
        .unwrap();
    let response = router.oneshot(request).await.expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(collection_rows(&test.pool).await.is_empty());
}

// ==================== UC-12: Delete a collection ====================

fn delete_request(uuid: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(format!("/v1/collections/{uuid}"))
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .body(Body::empty())
        .unwrap()
}

fn unauthenticated_delete_request(uuid: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(format!("/v1/collections/{uuid}"))
        .body(Body::empty())
        .unwrap()
}

/// Insert a minimal `files` row linked to the collection identified by
/// `collection_uuid`, as UC-13 will once it ships. Used to assert UC-12
/// unlinks rather than deletes contained items.
async fn seed_linked_file(pool: &SqlitePool, collection_uuid: &str) -> String {
    let file_uuid = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO files (uuid, path, name, type, content_hash, indexed_at, collection_id) \
         VALUES (?, ?, ?, 'text', 'hash', ?, \
         (SELECT id FROM collections WHERE uuid = ?))",
    )
    .bind(&file_uuid)
    .bind(format!("/lib/{file_uuid}.txt"))
    .bind("note.txt")
    .bind(chrono::Utc::now().to_rfc3339())
    .bind(collection_uuid)
    .execute(pool)
    .await
    .expect("seed linked file");
    file_uuid
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_existing_collection_when_deleted_then_200_with_predelete_body_and_row_removed() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    let (router, uuid) = create_collection(router, "Sci-fi novels").await;

    let response = router.oneshot(delete_request(&uuid)).await.expect("delete");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["uuid"], uuid);
    assert_eq!(body["name"], "Sci-fi novels");

    assert!(collection_rows(&test.pool).await.is_empty());
}

#[tokio::test]
async fn given_collection_with_linked_file_when_deleted_then_file_unlinked_not_removed() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    let (router, uuid) = create_collection(router, "Sci-fi novels").await;
    let file_uuid = seed_linked_file(&test.pool, &uuid).await;

    let response = router.oneshot(delete_request(&uuid)).await.expect("delete");
    assert_eq!(response.status(), StatusCode::OK);

    let (kept_uuid, collection_id): (String, Option<i64>) =
        sqlx::query_as("SELECT uuid, collection_id FROM files WHERE uuid = ?")
            .bind(&file_uuid)
            .fetch_one(&test.pool)
            .await
            .expect("file row kept");
    assert_eq!(kept_uuid, file_uuid, "the file itself is preserved");
    assert_eq!(
        collection_id, None,
        "the file is unlinked from the collection"
    );
}

// ---------------- AF-01: not found ----------------

#[tokio::test]
async fn given_unknown_uuid_when_deleted_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(delete_request(&uuid::Uuid::new_v4().to_string()))
        .await
        .expect("delete");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn given_non_uuid_path_segment_when_deleted_then_400() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(delete_request("not-a-uuid"))
        .await
        .expect("delete");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ---------------- AF-02: unauthorized ----------------

#[tokio::test]
async fn given_no_token_when_deleted_then_401_and_collection_kept() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    let (router, uuid) = create_collection(router, "Sci-fi novels").await;

    let response = router
        .oneshot(unauthenticated_delete_request(&uuid))
        .await
        .expect("delete");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(collection_rows(&test.pool).await.len(), 1);
}

// ==================== UC-13: Add items to a collection ====================

fn add_items_request(uuid: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/v1/collections/{uuid}/items"))
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn unauthenticated_add_items_request(uuid: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/v1/collections/{uuid}/items"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Insert a minimal, unlinked `files` row and return its uuid.
async fn seed_standalone_file(pool: &SqlitePool) -> String {
    let file_uuid = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO files (uuid, path, name, type, content_hash, indexed_at) \
         VALUES (?, ?, ?, 'text', 'hash', ?)",
    )
    .bind(&file_uuid)
    .bind(format!("/lib/{file_uuid}.txt"))
    .bind("note.txt")
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(pool)
    .await
    .expect("seed standalone file");
    file_uuid
}

/// Create a `kind: bookmark` collection via the router and return its uuid.
async fn create_bookmark_collection(router: axum::Router, name: &str) -> (axum::Router, String) {
    let response = router
        .clone()
        .oneshot(create_request(json!({ "name": name, "kind": "bookmark" })))
        .await
        .expect("create bookmark collection");
    assert_eq!(response.status(), StatusCode::CREATED);
    let uuid = body_json(response).await["uuid"]
        .as_str()
        .unwrap()
        .to_string();
    (router, uuid)
}

/// Create a bookmark via the router and return its uuid.
async fn create_bookmark(router: axum::Router, url: &str, title: &str) -> (axum::Router, String) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/bookmarks")
                .header(
                    "authorization",
                    format!("Bearer {}", common::TEST_TOKEN).as_str(),
                )
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "url": url, "title": title }).to_string(),
                ))
                .unwrap(),
        )
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
async fn given_file_collection_and_existing_files_when_posted_then_200_and_files_linked() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, collection_uuid) = create_collection(router, "My files").await;
    let file_uuid = seed_standalone_file(&test.pool).await;

    let response = router
        .oneshot(add_items_request(
            &collection_uuid,
            json!({ "itemUuids": [file_uuid] }),
        ))
        .await
        .expect("add items");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["collectionUuid"], collection_uuid);
    assert_eq!(
        body["items"],
        json!([{"itemUuid": file_uuid, "added": true}])
    );

    let (linked_collection_id, collection_id): (Option<i64>, i64) = {
        let linked: Option<i64> =
            sqlx::query_scalar("SELECT collection_id FROM files WHERE uuid = ?")
                .bind(&file_uuid)
                .fetch_one(&test.pool)
                .await
                .unwrap();
        let cid: i64 = sqlx::query_scalar("SELECT id FROM collections WHERE uuid = ?")
            .bind(&collection_uuid)
            .fetch_one(&test.pool)
            .await
            .unwrap();
        (linked, cid)
    };
    assert_eq!(linked_collection_id, Some(collection_id));
}

#[tokio::test]
async fn given_bookmark_collection_and_existing_bookmark_when_posted_then_200_and_bookmark_linked()
{
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, collection_uuid) = create_bookmark_collection(router, "Reading list").await;
    let (router, bookmark_uuid) = create_bookmark(router, "https://example.com", "Example").await;

    let response = router
        .oneshot(add_items_request(
            &collection_uuid,
            json!({ "itemUuids": [bookmark_uuid] }),
        ))
        .await
        .expect("add items");

    assert_eq!(response.status(), StatusCode::OK);

    let linked: Option<i64> =
        sqlx::query_scalar("SELECT collection_id FROM bookmarks WHERE uuid = ?")
            .bind(&bookmark_uuid)
            .fetch_one(&test.pool)
            .await
            .unwrap();
    assert!(linked.is_some());
}

// ---------------- AF-01: item type does not match collection kind ----------------

#[tokio::test]
async fn given_bookmark_item_for_file_collection_when_posted_then_reported_wrong_kind() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, collection_uuid) = create_collection(router, "My files").await;
    let (router, bookmark_uuid) = create_bookmark(router, "https://example.com", "Example").await;

    let response = router
        .oneshot(add_items_request(
            &collection_uuid,
            json!({ "itemUuids": [bookmark_uuid] }),
        ))
        .await
        .expect("add items");

    // The request succeeded; the item is what was rejected, and the response
    // says which kind of mistake it was.
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(
        body["items"],
        json!([{"itemUuid": bookmark_uuid, "added": false, "reason": "wrong_kind"}])
    );

    let linked: Option<i64> =
        sqlx::query_scalar("SELECT collection_id FROM bookmarks WHERE uuid = ?")
            .bind(&bookmark_uuid)
            .fetch_one(&test.pool)
            .await
            .unwrap();
    assert_eq!(linked, None);
}

// ---------------- AF-02: referenced item does not exist ----------------

#[tokio::test]
async fn given_unknown_item_uuid_when_posted_then_reported_not_found() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    let (router, collection_uuid) = create_collection(router, "My files").await;
    let unknown = uuid::Uuid::new_v4().to_string();

    let response = router
        .oneshot(add_items_request(
            &collection_uuid,
            json!({ "itemUuids": [unknown] }),
        ))
        .await
        .expect("add items");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(
        body["items"],
        json!([{"itemUuid": unknown, "added": false, "reason": "not_found"}]),
        "told apart from the wrong-kind rejection: this uuid names nothing"
    );
}

#[tokio::test]
async fn given_empty_item_uuids_when_posted_then_400() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    let (router, collection_uuid) = create_collection(router, "My files").await;

    let response = router
        .oneshot(add_items_request(
            &collection_uuid,
            json!({ "itemUuids": [] }),
        ))
        .await
        .expect("add items");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ---------------- AF-03: collection does not exist ----------------

#[tokio::test]
async fn given_unknown_collection_uuid_when_posted_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(add_items_request(
            &uuid::Uuid::new_v4().to_string(),
            json!({ "itemUuids": [uuid::Uuid::new_v4().to_string()] }),
        ))
        .await
        .expect("add items");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------- AF-04: unauthorized ----------------

#[tokio::test]
async fn given_no_token_when_posted_items_then_401() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    let (router, collection_uuid) = create_collection(router, "My files").await;

    let response = router
        .oneshot(unauthenticated_add_items_request(
            &collection_uuid,
            json!({ "itemUuids": [uuid::Uuid::new_v4().to_string()] }),
        ))
        .await
        .expect("add items");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ==================== UC-14: Remove and list items in a collection ====================

fn remove_item_request(collection_uuid: &str, item_uuid: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(format!(
            "/v1/collections/{collection_uuid}/items/{item_uuid}"
        ))
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .body(Body::empty())
        .unwrap()
}

fn unauthenticated_remove_item_request(collection_uuid: &str, item_uuid: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(format!(
            "/v1/collections/{collection_uuid}/items/{item_uuid}"
        ))
        .body(Body::empty())
        .unwrap()
}

fn list_items_request(collection_uuid: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(format!("/v1/collections/{collection_uuid}/items"))
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .body(Body::empty())
        .unwrap()
}

async fn add_items(router: axum::Router, collection_uuid: &str, item_uuid: &str) -> axum::Router {
    let response = router
        .clone()
        .oneshot(add_items_request(
            collection_uuid,
            json!({ "itemUuids": [item_uuid] }),
        ))
        .await
        .expect("add items");
    assert_eq!(response.status(), StatusCode::OK);
    router
}

// ---------------- Main flow: remove ----------------

#[tokio::test]
async fn given_linked_file_when_removed_then_200_and_unlinked() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, collection_uuid) = create_collection(router, "My files").await;
    let file_uuid = seed_standalone_file(&test.pool).await;
    let router = add_items(router, &collection_uuid, &file_uuid).await;

    let response = router
        .oneshot(remove_item_request(&collection_uuid, &file_uuid))
        .await
        .expect("remove item");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["collectionUuid"], collection_uuid);
    assert_eq!(body["itemUuid"], file_uuid);

    let linked: Option<i64> = sqlx::query_scalar("SELECT collection_id FROM files WHERE uuid = ?")
        .bind(&file_uuid)
        .fetch_one(&test.pool)
        .await
        .unwrap();
    assert_eq!(linked, None);
}

#[tokio::test]
async fn given_linked_bookmark_when_removed_then_200_and_unlinked() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, collection_uuid) = create_bookmark_collection(router, "Reading list").await;
    let (router, bookmark_uuid) = create_bookmark(router, "https://example.com", "Example").await;
    let router = add_items(router, &collection_uuid, &bookmark_uuid).await;

    let response = router
        .oneshot(remove_item_request(&collection_uuid, &bookmark_uuid))
        .await
        .expect("remove item");

    assert_eq!(response.status(), StatusCode::OK);
    let linked: Option<i64> =
        sqlx::query_scalar("SELECT collection_id FROM bookmarks WHERE uuid = ?")
            .bind(&bookmark_uuid)
            .fetch_one(&test.pool)
            .await
            .unwrap();
    assert_eq!(linked, None);
}

// ---------------- AF-01: item unknown or not in the collection ----------------

#[tokio::test]
async fn given_unknown_item_when_removed_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    let (router, collection_uuid) = create_collection(router, "My files").await;

    let response = router
        .oneshot(remove_item_request(
            &collection_uuid,
            &uuid::Uuid::new_v4().to_string(),
        ))
        .await
        .expect("remove item");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn given_item_not_linked_when_removed_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, collection_uuid) = create_collection(router, "My files").await;
    let file_uuid = seed_standalone_file(&test.pool).await;

    let response = router
        .oneshot(remove_item_request(&collection_uuid, &file_uuid))
        .await
        .expect("remove item");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------- AF-02: collection does not exist ----------------

#[tokio::test]
async fn given_unknown_collection_when_removed_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(remove_item_request(
            &uuid::Uuid::new_v4().to_string(),
            &uuid::Uuid::new_v4().to_string(),
        ))
        .await
        .expect("remove item");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------- AF-03: unauthorized ----------------

#[tokio::test]
async fn given_no_token_when_removed_then_401() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    let (router, collection_uuid) = create_collection(router, "My files").await;

    let response = router
        .oneshot(unauthenticated_remove_item_request(
            &collection_uuid,
            &uuid::Uuid::new_v4().to_string(),
        ))
        .await
        .expect("remove item");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ---------------- Main flow: list ----------------

#[tokio::test]
async fn given_file_collection_with_member_when_listed_then_200_with_files() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, collection_uuid) = create_collection(router, "My files").await;
    let file_uuid = seed_standalone_file(&test.pool).await;
    let router = add_items(router, &collection_uuid, &file_uuid).await;

    let response = router
        .oneshot(list_items_request(&collection_uuid))
        .await
        .expect("list items");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["collectionUuid"], collection_uuid);
    assert_eq!(body["kind"], "file");
    let items = body["items"].as_array().expect("array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["uuid"], file_uuid);
}

#[tokio::test]
async fn given_bookmark_collection_with_member_when_listed_then_200_with_bookmarks() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let (router, collection_uuid) = create_bookmark_collection(router, "Reading list").await;
    let (router, bookmark_uuid) = create_bookmark(router, "https://example.com", "Example").await;
    let router = add_items(router, &collection_uuid, &bookmark_uuid).await;

    let response = router
        .oneshot(list_items_request(&collection_uuid))
        .await
        .expect("list items");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["kind"], "bookmark");
    let items = body["items"].as_array().expect("array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["uuid"], bookmark_uuid);
}

#[tokio::test]
async fn given_empty_collection_when_listed_then_200_with_empty_array() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    let (router, collection_uuid) = create_collection(router, "My files").await;

    let response = router
        .oneshot(list_items_request(&collection_uuid))
        .await
        .expect("list items");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 0);
}

// ---------------- AF-01 (list): collection does not exist ----------------

#[tokio::test]
async fn given_unknown_collection_when_listed_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(list_items_request(&uuid::Uuid::new_v4().to_string()))
        .await
        .expect("list items");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------- AF-02 (list): unauthorized ----------------

#[tokio::test]
async fn given_no_token_when_listed_then_401() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    let (router, collection_uuid) = create_collection(router, "My files").await;

    let request = Request::builder()
        .method("GET")
        .uri(format!("/v1/collections/{collection_uuid}/items"))
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.expect("list items");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
