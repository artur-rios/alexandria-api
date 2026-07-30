mod common;

use std::collections::BTreeMap;

use alexandria_core::config::Settings;
use alexandria_http::app;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tempfile::tempdir;
use tower::ServiceExt;

use crate::common::{file_rows, file_rows_with_missing, test_app, wait_for_files};

fn index_request(root: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", "Bearer test-token")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "root": root }).to_string()))
        .unwrap()
}

fn refresh_request() -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/index/refresh")
        .header("authorization", "Bearer test-token")
        .body(Body::empty())
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

#[tokio::test]
async fn given_changed_and_deleted_files_when_refresh_posted_then_refreshes_and_marks_missing() {
    let lib = tempdir().unwrap();
    let a = common::write_file(&lib, "a.mp3", b"audio-v1");
    let _b = common::write_file(&lib, "b.md", b"text-v1");

    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let _ = router
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("index one-shot");
    wait_for_files(&test.pool, 2).await;

    let before = file_rows(&test.pool).await;
    assert_eq!(before.len(), 2);
    let old_a_hash = before
        .iter()
        .find(|r| r.1 == "a.mp3")
        .map(|r| r.3.clone())
        .expect("a indexed");
    let old_b_hash = before
        .iter()
        .find(|r| r.1 == "b.md")
        .map(|r| r.3.clone())
        .expect("b indexed");

    // Mutate: change a's bytes on disk, delete b from disk.
    std::fs::write(&a, b"audio-v2-CHANGED").unwrap();
    std::fs::remove_file(lib.path().join("b.md")).unwrap();

    let router2 = app(Settings::default(), test.services.clone());
    let response = router2.oneshot(refresh_request()).await.expect("refresh one-shot");
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert!(body["runId"].as_str().unwrap().is_empty() == false);

    // Wait for the refresh to settle: a's hash must differ from the old one,
    // and b must have a missing_at set.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let rows = file_rows_with_missing(&test.pool).await;
        let a_row = rows.iter().find(|r| r.1 == "a.mp3").expect("a");
        let b_row = rows.iter().find(|r| r.1 == "b.md").expect("b");
        let a_refreshed = a_row.3 != old_a_hash;
        let b_marked = b_row.4.is_some();
        if a_refreshed && b_marked {
            assert_ne!(a_row.3, old_a_hash, "a hash refreshed");
            assert_eq!(a_row.4, None, "a missing marker cleared");
            assert_eq!(b_row.3, old_b_hash, "b hash untouched when missing");
            assert!(b_row.4.is_some(), "b marked missing");
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "refresh never settled; a refreshed={a_refreshed}, b marked={b_marked}"
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn given_no_bearer_when_refresh_posted_then_returns_401() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let request = Request::builder()
        .method("POST")
        .uri("/v1/index/refresh")
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.expect("one-shot");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}