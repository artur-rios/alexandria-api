//! UC-42 integration tests for `GET /v1/index/runs/{runId}` (Testing
//! Specification §7): the real axum router over a real temp SQLite database.

mod common;

use alexandria_core::config::Settings;
use alexandria_http::app;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use crate::common::{test_app, TEST_TOKEN};

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
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

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
    // A completed refresh carries its four counts and no index counts.
    for field in ["refreshed", "markedMissing", "unchanged", "failed"] {
        assert!(run[field].is_number(), "missing {field}: {run}");
    }
    assert!(run["scanned"].is_null(), "index counts must not appear");
    assert!(run["root"].is_null(), "a refresh carries no root");
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
