//! UC-34/UC-35 integration tests for `POST /v1/auth/local/login` and
//! `POST /v1/auth/local/credentials` (Testing Specification §7): the real
//! axum router over a real temp SQLite database. `common::test_app()` runs
//! in local auth mode (AF-01's wrong-mode branch is unit-tested instead —
//! there is no way to flip the active mode mid-suite here).

mod common;

use alexandria_core::config::Settings;
use alexandria_http::app;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::common::test_app;

fn credentials_request(body: Value, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/auth/local/credentials")
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

/// Create the account the way a client now does (UC-41), so the UC-35
/// tests below have credentials to change.
fn register_request(email: &str, password: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/auth/local/register")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "email": email,
                "password": password,
                "passwordConfirmation": password,
            })
            .to_string(),
        ))
        .unwrap()
}

fn login_request(body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/auth/local/login")
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

// ---------------- UC-35 main flow ----------------

#[tokio::test]
async fn given_no_credentials_yet_when_set_posted_unauthenticated_then_401() {
    // UC-35 is change-only since UC-41; bootstrap is `/register`.
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(credentials_request(
            json!({ "email": "owner@example.com", "password": "correct horse battery" }),
            None,
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn given_existing_credentials_and_valid_token_when_set_posted_then_200_and_changed() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let first = router
        .clone()
        .oneshot(register_request(
            "owner@example.com",
            "correct horse battery",
        ))
        .await
        .expect("register one-shot");
    assert_eq!(first.status(), StatusCode::CREATED);

    let response = router
        .oneshot(credentials_request(
            json!({ "email": "new-owner@example.com", "password": "another good passphrase" }),
            Some(common::TEST_TOKEN),
        ))
        .await
        .expect("change one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["email"], "new-owner@example.com");
}

// ---------------- UC-35 AF-03: conditional authorization ----------------

#[tokio::test]
async fn given_existing_credentials_and_no_token_when_set_posted_then_401_and_unchanged() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let first = router
        .clone()
        .oneshot(register_request(
            "owner@example.com",
            "correct horse battery",
        ))
        .await
        .expect("register one-shot");
    assert_eq!(first.status(), StatusCode::CREATED);

    let response = router
        .oneshot(credentials_request(
            json!({ "email": "attacker@example.com", "password": "another good passphrase" }),
            None,
        ))
        .await
        .expect("change one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ---------------- UC-35 AF-02: invalid input ----------------

#[tokio::test]
async fn given_empty_password_when_set_posted_then_400() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let first = router
        .clone()
        .oneshot(register_request(
            "owner@example.com",
            "correct horse battery",
        ))
        .await
        .expect("register one-shot");
    assert_eq!(first.status(), StatusCode::CREATED);

    let response = router
        .oneshot(credentials_request(
            json!({ "email": "owner@example.com", "password": "" }),
            Some(common::TEST_TOKEN),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ---------------- UC-34 main flow ----------------

#[tokio::test]
async fn given_correct_credentials_when_login_posted_then_200_with_session_id() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let setup = router
        .clone()
        .oneshot(register_request(
            "owner@example.com",
            "correct horse battery",
        ))
        .await
        .expect("register one-shot");
    assert_eq!(setup.status(), StatusCode::CREATED);

    let response = router
        .oneshot(login_request(
            json!({ "email": "owner@example.com", "password": "correct horse battery" }),
        ))
        .await
        .expect("login one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["success"], true);
    assert!(
        uuid::Uuid::parse_str(body["sessionId"].as_str().expect("sessionId")).is_ok(),
        "sessionId is a uuid"
    );
}

// ---------------- UC-34 AF-02: wrong credentials ----------------

#[tokio::test]
async fn given_wrong_password_when_login_posted_then_401() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let setup = router
        .clone()
        .oneshot(register_request(
            "owner@example.com",
            "correct horse battery",
        ))
        .await
        .expect("register one-shot");
    assert_eq!(setup.status(), StatusCode::CREATED);

    let response = router
        .oneshot(login_request(
            json!({ "email": "owner@example.com", "password": "wrong" }),
        ))
        .await
        .expect("login one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ---------------- UC-34 AF-03: no credentials set ----------------

#[tokio::test]
async fn given_no_credentials_set_when_login_posted_then_500() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(login_request(
            json!({ "email": "owner@example.com", "password": "correct horse battery" }),
        ))
        .await
        .expect("login one-shot");

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}
