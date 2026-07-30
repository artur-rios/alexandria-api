use alexandria_core::config::Settings;
use alexandria_http::app;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

#[tokio::test]
async fn given_running_app_when_health_requested_then_returns_ok_contract() {
    let settings = Settings::default();
    let router = app(settings);

    let response = router
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);

    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: Value = serde_json::from_slice(&bytes).expect("json");

    assert_eq!(json["status"], "ok");
    assert_eq!(json["database"], "reachable");
    assert_eq!(json["filesystem"], "reachable");
    assert_eq!(json["authMode"], "external");
}
