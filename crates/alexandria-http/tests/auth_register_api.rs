//! UC-41 integration tests for `POST /v1/auth/local/register` (Testing
//! Specification §7): the real axum router over a real temp SQLite
//! database. `common::test_app()` runs in local auth mode, so AF-01's
//! wrong-mode branch is unit-tested instead — there is no way to flip the
//! active mode mid-suite here.

mod common;

use alexandria_core::config::Settings;
use alexandria_http::app;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::common::test_app;

const PASSWORD: &str = "correct horse battery";

fn register_request(body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/auth/local/register")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

const ORIGINAL_EMAIL: &str = "owner@example.com";

fn valid_body() -> Value {
    json!({
        "email": ORIGINAL_EMAIL,
        "password": PASSWORD,
        "passwordConfirmation": PASSWORD,
    })
}

fn login_request(email: &str, password: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/auth/local/login")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "email": email, "password": password }).to_string(),
        ))
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
async fn given_no_account_when_register_posted_unauthenticated_then_201_with_session() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(register_request(valid_body()))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    assert_eq!(body["success"], true);
    assert_eq!(body["email"], "owner@example.com");
    assert!(
        body["sessionId"].as_str().is_some_and(|id| !id.is_empty()),
        "a session id must be returned: {body}"
    );
}

#[tokio::test]
async fn given_a_registration_session_when_used_on_a_gated_route_then_authenticated() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .clone()
        .oneshot(register_request(valid_body()))
        .await
        .expect("register one-shot");
    assert_eq!(response.status(), StatusCode::CREATED);
    let session_id = body_json(response).await["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    let gated = Request::builder()
        .method("GET")
        .uri("/v1/files")
        .header("authorization", format!("Bearer {session_id}"))
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(gated).await.expect("gated one-shot");

    assert_ne!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "the registration session must authenticate immediately"
    );
}

// ---------------- AF-02: already registered ----------------

#[tokio::test]
async fn given_an_existing_account_when_register_posted_then_409() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let first = router
        .clone()
        .oneshot(register_request(valid_body()))
        .await
        .expect("first one-shot");
    assert_eq!(first.status(), StatusCode::CREATED);

    let response = router
        .clone()
        .oneshot(register_request(json!({
            "email": "someone-else@example.com",
            "password": PASSWORD,
            "passwordConfirmation": PASSWORD,
        })))
        .await
        .expect("second one-shot");

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = body_json(response).await;
    assert_eq!(
        body["error"], "a local account already exists",
        "the response must carry the account-already-exists message: {body}"
    );
    // Not every failure has a stable reason code yet — issue #101 adopts them
    // for input rejections first. A failure without one must render exactly
    // as it always has, or an existing client breaks on an envelope that grew
    // members it never asked for.
    assert_eq!(
        body.as_object().expect("object").len(),
        1,
        "an error with no code must render only `error`: {body}"
    );

    // The spec's test plan calls for "409 on a second registration with
    // the stored credentials unchanged" — prove it by logging in with the
    // *original* email and password, not the rejected second submission.
    let login_response = router
        .oneshot(login_request(ORIGINAL_EMAIL, PASSWORD))
        .await
        .expect("login one-shot");
    assert_eq!(
        login_response.status(),
        StatusCode::OK,
        "the original credentials must still work after the rejected second registration"
    );
}

// ---------------- AF-03 / AF-04 / AF-05: input rules ----------------

#[tokio::test]
async fn given_a_malformed_email_when_register_posted_then_400() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(register_request(json!({
            "email": "not-an-email",
            "password": PASSWORD,
            "passwordConfirmation": PASSWORD,
        })))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(body["code"], "email_malformed");
}

#[tokio::test]
async fn given_a_weak_password_when_register_posted_then_400() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(register_request(json!({
            "email": "owner@example.com",
            "password": "short",
            "passwordConfirmation": "short",
        })))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // Issue #101: the reason travels as a code and its bound, not only as an
    // English sentence, so a client can say which rule failed in its own
    // language without restating the policy.
    let body = body_json(response).await;
    assert_eq!(body["code"], "password_too_short");
    assert_eq!(body["params"]["min"], "12");
    assert_eq!(body["error"], "password must be at least 12 characters");
}

#[tokio::test]
async fn given_a_mismatched_confirmation_when_register_posted_then_400() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(register_request(json!({
            "email": "owner@example.com",
            "password": PASSWORD,
            "passwordConfirmation": "correct horse batteries",
        })))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(body["code"], "password_confirmation_mismatch");
}

#[tokio::test]
async fn given_a_body_missing_the_confirmation_when_register_posted_then_400() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(register_request(json!({
            "email": "owner@example.com",
            "password": PASSWORD,
        })))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // An unreadable body is its own reason: a client cannot tell it from a
    // policy rejection by the status alone, since both are `400`.
    let body = body_json(response).await;
    assert_eq!(body["code"], "malformed_body");
}
