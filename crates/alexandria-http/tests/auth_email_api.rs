//! Integration tests for the confirmation and password-reset endpoints
//! (issue #102), over the real axum router and a real temp SQLite database.
//!
//! One thing shapes every test here: the wiring under test uses the only mail
//! provider that exists today, `UnconfiguredMailSender`, which never sends.
//! So the paths that *require* a delivered secret are driven by seeding the
//! token row the way a real send would have — the endpoint under test still
//! does all of its own work, and the only thing stubbed out is the transport
//! that is not built yet. The refusal paths need no such help: they are what
//! every install actually gets until the mail integration ships.

mod common;

use alexandria_core::auth::tokens::hash_token;
use alexandria_core::config::Settings;
use alexandria_http::app;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use chrono::{Duration, Utc};
use serde_json::{json, Value};
use sqlx::sqlite::SqlitePool;
use tower::ServiceExt;

use crate::common::{test_app, TEST_TOKEN};

const EMAIL: &str = "owner@example.com";
const PASSWORD: &str = "correct horse battery";
const NEW_PASSWORD: &str = "a quite different passphrase";

fn post(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn authenticated(method: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap()
}

fn register_body() -> Value {
    json!({
        "email": EMAIL,
        "password": PASSWORD,
        "passwordConfirmation": PASSWORD,
    })
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

/// Seed the token row a successful send would have written, so an endpoint
/// that consumes a secret can be exercised end-to-end while the transport that
/// would carry it does not exist yet.
async fn seed_token(pool: &SqlitePool, purpose: &str, secret: &str, expires_in: Duration) {
    let now = Utc::now();
    sqlx::query(
        "INSERT INTO auth_tokens (purpose, token_hash, email, created_at, expires_at) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(purpose)
    .bind(hash_token(secret))
    .bind(EMAIL)
    .bind(now.to_rfc3339())
    .bind((now + expires_in).to_rfc3339())
    .execute(pool)
    .await
    .expect("seed token");
}

// ---------------- Registration reports delivery (UC-01 AF-06) ----------------

#[tokio::test]
async fn given_no_mail_transport_when_registered_then_created_and_reported_unsent() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(post("/v1/auth/local/register", register_body()))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    assert_eq!(body["success"], json!(true));
    assert_eq!(body["emailConfirmed"], json!(false));
    assert_eq!(body["confirmationSent"], json!(false));
    assert_eq!(body["confirmationError"], "mail_not_configured");
    assert!(
        body["sessionId"].is_string(),
        "the account is created and the session opened regardless: {body}"
    );
}

// ---------------- Account state (FR-AU-13) ----------------

#[tokio::test]
async fn given_a_new_account_when_state_read_then_unconfirmed() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    router
        .clone()
        .oneshot(post("/v1/auth/local/register", register_body()))
        .await
        .expect("register");

    let response = router
        .oneshot(authenticated("GET", "/v1/auth/local/account"))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["email"], EMAIL);
    assert_eq!(body["emailConfirmed"], json!(false));
}

#[tokio::test]
async fn given_no_token_when_state_read_then_401() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    router
        .clone()
        .oneshot(post("/v1/auth/local/register", register_body()))
        .await
        .expect("register");

    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/auth/local/account")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ---------------- Confirm (FR-AU-14) ----------------

#[tokio::test]
async fn given_a_seeded_code_when_confirmed_then_the_account_reports_confirmed() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    router
        .clone()
        .oneshot(post("/v1/auth/local/register", register_body()))
        .await
        .expect("register");
    seed_token(
        &test.pool,
        "email_confirmation",
        "TESTCODE",
        Duration::hours(24),
    )
    .await;

    let response = router
        .clone()
        .oneshot(post(
            "/v1/auth/local/email/confirm",
            json!({ "code": "TESTCODE" }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["emailConfirmed"], json!(true));

    // And the state the front-end's catalog lock reads now agrees.
    let account = router
        .oneshot(authenticated("GET", "/v1/auth/local/account"))
        .await
        .expect("one-shot");
    assert_eq!(body_json(account).await["emailConfirmed"], json!(true));
}

#[tokio::test]
async fn given_an_unknown_code_when_confirmed_then_400_with_its_reason() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    router
        .clone()
        .oneshot(post("/v1/auth/local/register", register_body()))
        .await
        .expect("register");

    let response = router
        .oneshot(post(
            "/v1/auth/local/email/confirm",
            json!({ "code": "NOTACODE" }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(response).await["code"], "confirmation_invalid");
}

#[tokio::test]
async fn given_an_expired_code_when_confirmed_then_400_expired() {
    // The three refusals are distinct so a client can say "ask for another"
    // instead of "that is wrong".
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    router
        .clone()
        .oneshot(post("/v1/auth/local/register", register_body()))
        .await
        .expect("register");
    seed_token(
        &test.pool,
        "email_confirmation",
        "OLDCODE1",
        Duration::hours(-1),
    )
    .await;

    let response = router
        .oneshot(post(
            "/v1/auth/local/email/confirm",
            json!({ "code": "OLDCODE1" }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(response).await["code"], "confirmation_expired");
}

#[tokio::test]
async fn given_a_spent_code_when_confirmed_on_an_unconfirmed_account_then_400_already_used() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    router
        .clone()
        .oneshot(post("/v1/auth/local/register", register_body()))
        .await
        .expect("register");
    seed_token(
        &test.pool,
        "email_confirmation",
        "USEDCODE",
        Duration::hours(24),
    )
    .await;
    sqlx::query("UPDATE auth_tokens SET consumed_at = ?")
        .bind(Utc::now().to_rfc3339())
        .execute(&test.pool)
        .await
        .expect("spend the code");

    let response = router
        .oneshot(post(
            "/v1/auth/local/email/confirm",
            json!({ "code": "USEDCODE" }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(response).await["code"],
        "confirmation_already_used"
    );
}

// ---------------- Resend (FR-AU-15) ----------------

#[tokio::test]
async fn given_no_mail_transport_when_resend_posted_then_503_with_its_reason() {
    // Every install today. An honest refusal, not a success that delivers
    // nothing.
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    router
        .clone()
        .oneshot(post("/v1/auth/local/register", register_body()))
        .await
        .expect("register");

    let response = router
        .oneshot(authenticated("POST", "/v1/auth/local/email/resend"))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body_json(response).await["code"], "mail_not_configured");
}

#[tokio::test]
async fn given_a_confirmed_account_when_resend_posted_then_409() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    router
        .clone()
        .oneshot(post("/v1/auth/local/register", register_body()))
        .await
        .expect("register");
    seed_token(
        &test.pool,
        "email_confirmation",
        "TESTCODE",
        Duration::hours(24),
    )
    .await;
    router
        .clone()
        .oneshot(post(
            "/v1/auth/local/email/confirm",
            json!({ "code": "TESTCODE" }),
        ))
        .await
        .expect("confirm");

    let response = router
        .oneshot(authenticated("POST", "/v1/auth/local/email/resend"))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::CONFLICT);
}

// ---------------- Password reset (FR-AU-16) ----------------

#[tokio::test]
async fn given_a_registered_and_an_unregistered_address_when_reset_requested_then_identical() {
    // The endpoint is unauthenticated; an answer that differed would be an
    // oracle for whether a given person owns this library. With no transport
    // both answer `503`, and that is itself the property under test: the
    // outcome does not vary with the address.
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    router
        .clone()
        .oneshot(post("/v1/auth/local/register", register_body()))
        .await
        .expect("register");

    let registered = router
        .clone()
        .oneshot(post(
            "/v1/auth/local/password/reset",
            json!({ "email": EMAIL }),
        ))
        .await
        .expect("one-shot");
    let registered_status = registered.status();
    let registered_body = body_json(registered).await;

    let stranger = router
        .oneshot(post(
            "/v1/auth/local/password/reset",
            json!({ "email": "someone-else@example.com" }),
        ))
        .await
        .expect("one-shot");
    let stranger_status = stranger.status();
    let stranger_body = body_json(stranger).await;

    assert_eq!(registered_status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(registered_status, stranger_status);
    assert_eq!(registered_body, stranger_body);
}

#[tokio::test]
async fn given_a_seeded_token_when_reset_completed_then_the_new_password_logs_in() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    router
        .clone()
        .oneshot(post("/v1/auth/local/register", register_body()))
        .await
        .expect("register");
    seed_token(
        &test.pool,
        "password_reset",
        "reset-token-value",
        Duration::minutes(60),
    )
    .await;

    let response = router
        .clone()
        .oneshot(post(
            "/v1/auth/local/password/reset/complete",
            json!({
                "token": "reset-token-value",
                "password": NEW_PASSWORD,
                "passwordConfirmation": NEW_PASSWORD,
            }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["email"], EMAIL);

    // The proof the reset actually took: the new password authenticates.
    let login = router
        .clone()
        .oneshot(post(
            "/v1/auth/local/login",
            json!({ "email": EMAIL, "password": NEW_PASSWORD }),
        ))
        .await
        .expect("one-shot");
    assert_eq!(login.status(), StatusCode::OK);

    // And the old one no longer does.
    let old = router
        .oneshot(post(
            "/v1/auth/local/login",
            json!({ "email": EMAIL, "password": PASSWORD }),
        ))
        .await
        .expect("one-shot");
    assert_eq!(old.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn given_a_completed_reset_when_an_older_session_is_used_then_401() {
    // A reset is what an owner does when someone else may hold their
    // credentials; every session goes, including the seeded test one.
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    router
        .clone()
        .oneshot(post("/v1/auth/local/register", register_body()))
        .await
        .expect("register");
    seed_token(
        &test.pool,
        "password_reset",
        "reset-token-value",
        Duration::minutes(60),
    )
    .await;
    router
        .clone()
        .oneshot(post(
            "/v1/auth/local/password/reset/complete",
            json!({
                "token": "reset-token-value",
                "password": NEW_PASSWORD,
                "passwordConfirmation": NEW_PASSWORD,
            }),
        ))
        .await
        .expect("complete");

    let response = router
        .oneshot(authenticated("GET", "/v1/auth/local/account"))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn given_an_unknown_token_when_reset_completed_then_400_with_its_reason() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    router
        .clone()
        .oneshot(post("/v1/auth/local/register", register_body()))
        .await
        .expect("register");

    let response = router
        .oneshot(post(
            "/v1/auth/local/password/reset/complete",
            json!({
                "token": "not-a-token",
                "password": NEW_PASSWORD,
                "passwordConfirmation": NEW_PASSWORD,
            }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(response).await["code"], "reset_invalid");
}

#[tokio::test]
async fn given_a_weak_new_password_when_reset_completed_then_400_and_nothing_changes() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    router
        .clone()
        .oneshot(post("/v1/auth/local/register", register_body()))
        .await
        .expect("register");
    seed_token(
        &test.pool,
        "password_reset",
        "reset-token-value",
        Duration::minutes(60),
    )
    .await;

    let response = router
        .clone()
        .oneshot(post(
            "/v1/auth/local/password/reset/complete",
            json!({
                "token": "reset-token-value",
                "password": "short",
                "passwordConfirmation": "short",
            }),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(response).await["code"], "password_too_short");

    let login = router
        .oneshot(post(
            "/v1/auth/local/login",
            json!({ "email": EMAIL, "password": PASSWORD }),
        ))
        .await
        .expect("one-shot");
    assert_eq!(
        login.status(),
        StatusCode::OK,
        "the original password must still work after a refused reset"
    );
}
