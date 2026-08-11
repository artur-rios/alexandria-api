//! UC-39 integration tests for `GET /v1/files/{uuid}/pages/{page}`
//! (Testing Specification §7): the real axum router, a real temp SQLite
//! database, and a real on-disk CBZ (built with the `zip` and `image`
//! crates) indexed through `POST /v1/index`.

mod common;

use alexandria_core::config::Settings;
use alexandria_http::app;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use tempfile::tempdir;
use tower::ServiceExt;

use crate::common::{file_rows_with_uuid, test_app, wait_for_files};

fn index_request(root: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .header("content-type", "application/json")
        .body(Body::from(serde_json::json!({ "root": root }).to_string()))
        .unwrap()
}

fn page_request(uuid: &str, page: u32) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(format!("/v1/files/{uuid}/pages/{page}"))
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .body(Body::empty())
        .unwrap()
}

fn unauthenticated_page_request(uuid: &str, page: u32) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(format!("/v1/files/{uuid}/pages/{page}"))
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn given_cbz_when_first_page_requested_then_image_bytes_returned() {
    // Arrange — a CBZ whose pages are stored out of order.
    let lib = tempdir().unwrap();
    common::write_cbz(&lib, "issue.cbz", &["page002.jpg", "page001.jpg"]);

    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    router
        .clone()
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("index");
    wait_for_files(&test.pool, 1).await;
    let rows = file_rows_with_uuid(&test.pool).await;
    let uuid = rows[0].0.clone();

    // Act
    let response = router
        .oneshot(page_request(&uuid, 1))
        .await
        .expect("one-shot");

    // Assert — page 1 is page001.jpg despite being stored second.
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "image/jpeg"
    );
    // A comic entry is library-supplied and served undecoded, so its bytes
    // are as sniffable as any other; `nosniff` holds the browser to the MIME
    // the entry name resolved to.
    assert_eq!(
        response
            .headers()
            .get("x-content-type-options")
            .expect("nosniff header present"),
        "nosniff"
    );
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.to_vec(), common::jpeg_bytes_for("page001.jpg"));
}

#[tokio::test]
async fn given_cbz_when_page_out_of_range_then_bad_request() {
    // Arrange
    let lib = tempdir().unwrap();
    common::write_cbz(&lib, "issue.cbz", &["page001.jpg"]);

    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    router
        .clone()
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("index");
    wait_for_files(&test.pool, 1).await;
    let rows = file_rows_with_uuid(&test.pool).await;
    let uuid = rows[0].0.clone();

    // Act
    let response = router
        .oneshot(page_request(&uuid, 5))
        .await
        .expect("one-shot");

    // Assert
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn given_cbr_comic_when_page_requested_then_bad_request() {
    // Arrange — the file is a genuine comic; the RAR format is what is
    // unsupported, so this is 400 and not 404.
    let lib = tempdir().unwrap();
    common::write_file(&lib, "issue.cbr", b"not really rar");

    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    router
        .clone()
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("index");
    wait_for_files(&test.pool, 1).await;
    let rows = file_rows_with_uuid(&test.pool).await;
    let uuid = rows[0].0.clone();

    // Act
    let response = router
        .oneshot(page_request(&uuid, 1))
        .await
        .expect("one-shot");

    // Assert
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn given_non_comic_file_when_page_requested_then_bad_request() {
    // Arrange
    let lib = tempdir().unwrap();
    common::write_file(&lib, "notes.txt", b"text");

    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    router
        .clone()
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("index");
    wait_for_files(&test.pool, 1).await;
    let rows = file_rows_with_uuid(&test.pool).await;
    let uuid = rows[0].0.clone();

    // Act
    let response = router
        .oneshot(page_request(&uuid, 1))
        .await
        .expect("one-shot");

    // Assert
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn given_unauthenticated_caller_when_page_requested_then_unauthorized() {
    // Arrange
    let lib = tempdir().unwrap();
    common::write_cbz(&lib, "issue.cbz", &["page001.jpg"]);

    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    router
        .clone()
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("index");
    wait_for_files(&test.pool, 1).await;
    let rows = file_rows_with_uuid(&test.pool).await;
    let uuid = rows[0].0.clone();

    // Act
    let response = router
        .oneshot(unauthenticated_page_request(&uuid, 1))
        .await
        .expect("one-shot");

    // Assert
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
