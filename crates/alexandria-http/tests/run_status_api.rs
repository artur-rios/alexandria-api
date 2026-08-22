//! UC-42 integration tests for `GET /v1/index/runs/{runId}` (Testing
//! Specification §7): the real axum router over a real temp SQLite database.

mod common;

use alexandria_core::config::Settings;
use alexandria_http::app;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use crate::common::{file_rows_with_missing, test_app, wait_for_files, write_file, TEST_TOKEN};

fn run_request(run_id: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("GET")
        .uri(format!("/v1/index/runs/{run_id}"));
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder.body(Body::empty()).unwrap()
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

/// Start a refresh and return its run id.
async fn start_refresh(router: &axum::Router) -> String {
    let request = Request::builder()
        .method("POST")
        .uri("/v1/index/refresh")
        .header("authorization", format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(request).await.expect("refresh");
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    body_json(response).await["runId"]
        .as_str()
        .expect("runId")
        .to_string()
}

#[tokio::test]
async fn given_a_started_run_when_polled_to_completion_then_it_reports_complete_with_counts() {
    // The assertion this whole use case exists to make possible: a client can
    // wait for a run to finish instead of guessing from the catalog counts.
    //
    // A real library with a changed file and a deleted file is seeded first
    // so that "complete" can be checked against something: this is the same
    // assertion the UC-02 parity test could not previously make (design doc
    // "Testing" section) — that `complete` implies every per-file write, not
    // just some of them, has actually landed.
    let test = test_app().await;
    let lib = tempfile::tempdir().expect("lib tempdir");
    write_file(&lib, "a.mp3", b"audio-v1");
    write_file(&lib, "b.md", b"text-v1");
    let router = app(Settings::default(), test.services.clone());

    let index_req = Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({ "root": lib.path().to_str().unwrap() }).to_string(),
        ))
        .unwrap();
    let index_resp = router.clone().oneshot(index_req).await.expect("index");
    assert_eq!(index_resp.status(), StatusCode::ACCEPTED);
    wait_for_files(&test.pool, 2).await;

    write_file(&lib, "a.mp3", b"audio-v2-CHANGED");
    std::fs::remove_file(lib.path().join("b.md")).expect("remove b.md");

    let run_id = start_refresh(&router).await;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let run = loop {
        let response = router
            .clone()
            .oneshot(run_request(&run_id, Some(TEST_TOKEN)))
            .await
            .expect("status");
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        if body["status"] != "running" {
            break body;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "run never left the running state"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    };

    assert_eq!(run["runId"], run_id);
    assert_eq!(run["kind"], "refresh");
    assert_eq!(run["status"], "complete");
    assert!(run["finishedAt"].is_string());
    // A completed refresh carries its four counts and no index counts. Task
    // 4 made re-index compare `stat` (size + mtime) rather than a recomputed
    // hash — a's changed size is what `refreshed` counts here, not a hash
    // difference — so the exact tally is asserted, not just that the fields
    // are present.
    assert_eq!(run["refreshed"], 1, "a's changed size is detected via stat");
    assert_eq!(run["markedMissing"], 1);
    assert_eq!(run["unchanged"], 0);
    assert_eq!(run["failed"], 0);
    let obj = run.as_object().expect("response body is a JSON object");
    assert!(
        !obj.contains_key("scanned"),
        "index-only counts must be omitted from a refresh run, not sent as null: {run}"
    );
    assert!(
        !obj.contains_key("root"),
        "a refresh carries no root, and the key must be omitted, not sent as null: {run}"
    );

    // The assertion `complete` exists to make possible: the catalog rows are
    // fully settled, not just some of them. `RefreshHandler::refresh_one`
    // processes cataloged paths concurrently, so a status of `complete` must
    // mean *both* halves of the refresh landed — a's stat-detected change and
    // b's missing marker — not just whichever half happened to finish first.
    //
    // Task 3 stopped indexing from computing a hash at all, and Task 4's
    // refresh never computes a new one either: a detected change clears
    // `content_hash` rather than replacing it (FR-FC-10), so a.mp3's hash
    // here is asserted empty, not equal to some freshly recomputed SHA-256.
    let rows = file_rows_with_missing(&test.pool).await;
    let by_name: std::collections::BTreeMap<String, (String, Option<String>)> = rows
        .into_iter()
        .map(|(_, name, _, hash, missing_at)| (name, (hash, missing_at)))
        .collect();
    let (a_hash, a_missing) = by_name.get("a.mp3").expect("a.mp3 row");
    assert!(a_missing.is_none(), "a.mp3 is still on disk");
    assert!(
        a_hash.is_empty(),
        "refresh clears the hash rather than recomputing one"
    );
    let (_, b_missing) = by_name.get("b.md").expect("b.md row");
    assert!(b_missing.is_some(), "b.md must carry a missing marker");
}

#[tokio::test]
async fn given_an_unknown_run_id_when_read_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(run_request(
            "00000000-0000-4000-8000-000000000000",
            Some(TEST_TOKEN),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn given_a_malformed_run_id_when_read_then_400() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(run_request("not-a-uuid", Some(TEST_TOKEN)))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn given_no_token_when_read_then_401() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let run_id = start_refresh(&router).await;

    let response = router
        .oneshot(run_request(&run_id, None))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
