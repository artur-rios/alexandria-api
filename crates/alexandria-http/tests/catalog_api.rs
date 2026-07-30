mod common;

use std::collections::BTreeMap;

use alexandria_core::config::Settings;
use alexandria_http::app;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tempfile::tempdir;
use tower::ServiceExt;

use crate::common::{file_rows, test_app, wait_for_files};

fn index_request(root: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", "Bearer test-token")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "root": root }).to_string()))
        .unwrap()
}

#[tokio::test]
async fn given_supported_files_when_index_posted_then_returns_202_with_run_id() {
    let lib = tempdir().unwrap();
    common::write_file(&lib, "song.mp3", b"audio bytes");
    common::write_file(&lib, "notes.md", b"# title\n");

    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::ACCEPTED);

    let bytes = to_bytes(response.into_body(), usize::MAX).await.expect("body");
    let json: Value = serde_json::from_slice(&bytes).expect("json");
    let run_id = json["runId"].as_str().expect("runId string");
    assert!(!run_id.is_empty());

    wait_for_files(&test.pool, 2).await;
    let rows = file_rows(&test.pool).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].1, "notes.md");
    assert_eq!(rows[0].2, "text");
    assert_eq!(rows[1].1, "song.mp3");
    assert_eq!(rows[1].2, "audio");
    assert!(!rows[0].3.is_empty(), "content hash is stored");
}

#[tokio::test]
async fn given_indexed_path_when_reindex_posted_then_existing_path_skipped_no_duplicate() {
    let lib = tempdir().unwrap();
    common::write_file(&lib, "track.mp3", b"audio");

    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let first = router
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("one-shot");
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    wait_for_files(&test.pool, 1).await;

    // A second index run over the same root must not duplicate the path.
    let router2 = app(Settings::default(), test.services);
    let second = router2
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("one-shot");
    assert_eq!(second.status(), StatusCode::ACCEPTED);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files")
        .fetch_one(&test.pool)
        .await
        .expect("count");
    assert_eq!(count.0, 1, "no duplicate path row created");
}

#[tokio::test]
async fn given_missing_root_when_index_posted_then_returns_400() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(index_request("/this/does/not/exist"))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn given_no_bearer_when_index_posted_then_returns_401() {
    let lib = tempdir().unwrap();

    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let request = Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "root": lib.path().to_str().unwrap() }).to_string()))
        .unwrap();

    let response = router.oneshot(request).await.expect("one-shot");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn given_subtype_rows_when_indexed_then_each_file_has_subtype_row() {
    let lib = tempdir().unwrap();
    common::write_file(&lib, "a.mp3", b"x");
    common::write_file(&lib, "v.mp4", b"y");
    common::write_file(&lib, "p.pdf", b"z");

    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    let _ = router
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("one-shot");

    wait_for_files(&test.pool, 3).await;

    let audio: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM audio_files")
        .fetch_one(&test.pool)
        .await
        .expect("audio");
    let video: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM video_files")
        .fetch_one(&test.pool)
        .await
        .expect("video");
    let document: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM documents")
        .fetch_one(&test.pool)
        .await
        .expect("document");
    assert_eq!(audio.0, 1);
    assert_eq!(video.0, 1);
    assert_eq!(document.0, 1);

    let _: &BTreeMap<(), ()> = &BTreeMap::new(); // silence unused import if any
}