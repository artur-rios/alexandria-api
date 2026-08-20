//! UC-47 integration tests for `GET /v1/settings` (Testing Specification §7):
//! the real axum router over a real temp SQLite database. Covers the main flow,
//! the configured value rather than the default, and AF-01 (unauthorized).

mod common;

use alexandria_core::config::Settings;
use alexandria_http::app;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use crate::common::{test_app, test_app_with_settings};

fn settings_request() -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri("/v1/settings")
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .body(Body::empty())
        .unwrap()
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

#[tokio::test]
async fn given_the_default_settings_when_requested_then_200_with_the_window() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router.oneshot(settings_request()).await.expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["deletion"]["retentionDays"], 30);
}

/// The point of the use case: what this server enforces, not the default.
#[tokio::test]
async fn given_a_configured_window_when_requested_then_that_value_is_returned() {
    let mut settings = Settings::default();
    settings.deletion.retention_days = 7;
    let test = test_app_with_settings(settings).await;
    let router = app(Settings::default(), test.services);

    let response = router.oneshot(settings_request()).await.expect("one-shot");

    let body = body_json(response).await;
    assert_eq!(
        body["deletion"]["retentionDays"], 7,
        "the handler reports the settings the services were built with, \
         not the ones the router was handed"
    );
}

/// AF-01.
#[tokio::test]
async fn given_unauthenticated_when_requested_then_401() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let request = Request::builder()
        .method("GET")
        .uri("/v1/settings")
        .body(Body::empty())
        .unwrap();

    let response = router.oneshot(request).await.expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
