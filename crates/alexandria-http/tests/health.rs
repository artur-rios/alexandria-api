//! UC-37 integration tests for `GET /health` (Testing Specification §7):
//! the real axum router over a real temp SQLite database and filesystem
//! root. Covers the main flow (both reachable) and both alternative flows
//! (AF-01 database unreachable, AF-02 filesystem root unreachable).

use std::sync::Arc;

use alexandria_core::config::Settings;
use alexandria_core::migrate::migrate_database;
use alexandria_core::services::build_services;
use alexandria_http::app;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

fn health_request() -> Request<Body> {
    Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn given_reachable_database_and_filesystem_when_health_requested_then_ok_contract() {
    let db_dir = tempfile::tempdir().expect("tempdir");
    let db_path = db_dir.path().join("alexandria.sqlite");
    let pool = migrate_database(db_path.to_str().expect("path"))
        .await
        .expect("migrate");

    let fs_root = tempfile::tempdir().expect("fs root tempdir");
    let mut settings = Settings::default();
    settings.filesystem.root = fs_root.path().to_str().unwrap().to_string();

    let services = Arc::new(build_services(&settings, pool).await);
    let router = app(settings, services);

    let response = router.oneshot(health_request()).await.expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["status"], "ok");
    assert_eq!(json["database"], "reachable");
    assert_eq!(json["filesystem"], "reachable");
    assert_eq!(json["authMode"], "external");
}

// ---------------- AF-01: database unreachable ----------------

#[tokio::test]
async fn given_closed_database_when_health_requested_then_degraded_and_database_unreachable() {
    let db_dir = tempfile::tempdir().expect("tempdir");
    let db_path = db_dir.path().join("alexandria.sqlite");
    let pool = migrate_database(db_path.to_str().expect("path"))
        .await
        .expect("migrate");

    let fs_root = tempfile::tempdir().expect("fs root tempdir");
    let mut settings = Settings::default();
    settings.filesystem.root = fs_root.path().to_str().unwrap().to_string();

    let services = Arc::new(build_services(&settings, pool.clone()).await);
    pool.close().await;
    let router = app(settings, services);

    let response = router.oneshot(health_request()).await.expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK, "liveness stays 200");
    let json = body_json(response).await;
    assert_eq!(json["status"], "degraded");
    assert_eq!(json["database"], "unreachable");
    assert_eq!(json["filesystem"], "reachable");
}

// ---------------- AF-02: filesystem root unreachable ----------------

#[tokio::test]
async fn given_missing_filesystem_root_when_health_requested_then_degraded_and_filesystem_unreachable(
) {
    let db_dir = tempfile::tempdir().expect("tempdir");
    let db_path = db_dir.path().join("alexandria.sqlite");
    let pool = migrate_database(db_path.to_str().expect("path"))
        .await
        .expect("migrate");

    let mut settings = Settings::default();
    settings.filesystem.root = db_dir
        .path()
        .join("no-such-directory")
        .to_str()
        .unwrap()
        .to_string();

    let services = Arc::new(build_services(&settings, pool).await);
    let router = app(settings, services);

    let response = router.oneshot(health_request()).await.expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK, "liveness stays 200");
    let json = body_json(response).await;
    assert_eq!(json["status"], "degraded");
    assert_eq!(json["database"], "reachable");
    assert_eq!(json["filesystem"], "unreachable");
}
