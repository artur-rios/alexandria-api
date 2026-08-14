//! Unit tests for password reset — request and complete (issue #102 /
//! FR-AU-16), against trait fakes with no database.
//!
//! This is the flow that makes the owner's password recoverable at all.
//! Before it, a forgotten password meant a lost catalog.

use chrono::{Duration, TimeZone, Utc};

use alexandria_core::auth::commands::complete_password_reset::{
    CompletePasswordResetHandler, RESET_ALREADY_USED, RESET_EXPIRED, RESET_INVALID,
};
use alexandria_core::auth::commands::request_password_reset::RequestPasswordResetHandler;
use alexandria_core::auth::commands::resend_confirmation::ResendConfirmationHandler;
use alexandria_core::auth::local::{LocalCredentialRepository, SessionRepository};
use alexandria_core::auth::mail::{MailKind, MAIL_NOT_CONFIGURED};
use alexandria_core::auth::password::verify_password;
use alexandria_core::auth::tokens::hash_token;
use alexandria_core::catalog::clock::FixedClock;
use alexandria_core::config::AuthMode;
use alexandria_core::errors::DomainError;

use crate::common::{
    FailingMailSender, FakeAuth, FakeAuthTokenRepository, FakeLocalCredentialRepository,
    FakeMailSender, FakeSessionRepository,
};

const EMAIL: &str = "owner@example.com";
const NEW_PASSWORD: &str = "a quite different passphrase";
const RESET_TTL_MINUTES: u32 = 60;

fn clock_at(offset: Duration) -> FixedClock {
    FixedClock(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap() + offset)
}

fn clock() -> FixedClock {
    clock_at(Duration::zero())
}

fn credentials() -> FakeLocalCredentialRepository {
    let repo = FakeLocalCredentialRepository::new();
    repo.seed(EMAIL, "the-old-hash");
    repo
}

fn request_handler(
    credentials: FakeLocalCredentialRepository,
    tokens: FakeAuthTokenRepository,
    mail: FakeMailSender,
) -> RequestPasswordResetHandler<
    FakeLocalCredentialRepository,
    FakeAuthTokenRepository,
    FakeMailSender,
    FixedClock,
> {
    RequestPasswordResetHandler::new(
        credentials,
        tokens,
        mail,
        clock(),
        AuthMode::Local,
        RESET_TTL_MINUTES,
    )
}

fn complete_handler(
    credentials: FakeLocalCredentialRepository,
    sessions: FakeSessionRepository,
    tokens: FakeAuthTokenRepository,
    clock: FixedClock,
) -> CompletePasswordResetHandler<
    FakeLocalCredentialRepository,
    FakeSessionRepository,
    FakeAuthTokenRepository,
    FixedClock,
> {
    CompletePasswordResetHandler::new(credentials, sessions, tokens, clock, AuthMode::Local)
}

/// Request a reset and read the token off the message that was "delivered" —
/// the same path a real owner takes.
async fn issue_token(
    credentials: &FakeLocalCredentialRepository,
    tokens: &FakeAuthTokenRepository,
    mail: &FakeMailSender,
) -> String {
    request_handler(credentials.clone(), tokens.clone(), mail.clone())
        .request(EMAIL)
        .await
        .expect("request");
    let sent = mail.sent();
    let message = sent.last().expect("a message must have been sent");
    assert_eq!(message.kind, MailKind::PasswordReset);
    message.secret.clone()
}

fn rejection_code(err: &DomainError) -> &str {
    match err {
        DomainError::Rejected(rejection) | DomainError::Unavailable(rejection) => rejection.code,
        other => panic!("expected a rejection, got {other:?}"),
    }
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_a_valid_token_when_completed_then_the_password_is_replaced() {
    let credentials = credentials();
    let tokens = FakeAuthTokenRepository::new();
    let mail = FakeMailSender::new();
    let token = issue_token(&credentials, &tokens, &mail).await;

    let result = complete_handler(
        credentials.clone(),
        FakeSessionRepository::new(),
        tokens,
        clock(),
    )
    .complete(&token, NEW_PASSWORD.to_string(), NEW_PASSWORD.to_string())
    .await
    .expect("a freshly issued token must complete the reset");

    assert!(result.success);
    assert_eq!(result.email, EMAIL);
    let stored = credentials.get().await.unwrap().unwrap();
    assert!(
        verify_password(NEW_PASSWORD, &stored.password_hash),
        "the new password must verify against the stored hash"
    );
}

#[tokio::test]
async fn given_open_sessions_when_a_reset_completes_then_every_session_is_gone() {
    // A reset is what an owner does when they believe someone else may hold
    // their credentials; leaving that someone's session open would defeat it.
    let credentials = credentials();
    let tokens = FakeAuthTokenRepository::new();
    let mail = FakeMailSender::new();
    let sessions = FakeSessionRepository::new();
    sessions
        .create_session(
            uuid::Uuid::new_v4(),
            clock_at(Duration::zero()).0,
            clock_at(Duration::hours(24)).0,
        )
        .await
        .expect("seed a session");
    assert_eq!(sessions.count(), 1);

    let token = issue_token(&credentials, &tokens, &mail).await;
    complete_handler(credentials, sessions.clone(), tokens, clock())
        .complete(&token, NEW_PASSWORD.to_string(), NEW_PASSWORD.to_string())
        .await
        .expect("complete");

    assert_eq!(sessions.count(), 0);
}

#[tokio::test]
async fn given_a_reset_token_when_stored_then_only_its_hash_is_written() {
    let credentials = credentials();
    let tokens = FakeAuthTokenRepository::new();
    let mail = FakeMailSender::new();
    let token = issue_token(&credentials, &tokens, &mail).await;

    let stored = tokens.stored_hashes();
    assert!(
        !stored.iter().any(|value| value == &token),
        "the plaintext token must never be stored"
    );
    assert!(stored.contains(&hash_token(&token)));
}

// ---------------- Request: the address is never revealed ----------------

#[tokio::test]
async fn given_an_unregistered_address_when_a_reset_is_requested_then_the_same_answer() {
    // The endpoint is unauthenticated. An answer that differed would tell
    // anyone who asked whether a given person owns this library.
    let credentials = credentials();
    let tokens = FakeAuthTokenRepository::new();
    let mail = FakeMailSender::new();

    let registered = request_handler(credentials.clone(), tokens.clone(), mail.clone())
        .request(EMAIL)
        .await
        .expect("registered address");
    let stranger = request_handler(credentials, tokens.clone(), mail.clone())
        .request("someone-else@example.com")
        .await
        .expect("unregistered address");

    assert_eq!(registered, stranger, "the two outcomes must be identical");
    assert_eq!(
        mail.sent().len(),
        1,
        "only the registered address may receive anything"
    );
}

#[tokio::test]
async fn given_no_account_at_all_when_a_reset_is_requested_then_the_same_answer() {
    let tokens = FakeAuthTokenRepository::new();
    let mail = FakeMailSender::new();

    let result = request_handler(FakeLocalCredentialRepository::new(), tokens, mail.clone())
        .request(EMAIL)
        .await
        .expect("an install with no account must not answer differently");

    assert!(result.success);
    assert!(mail.sent().is_empty());
}

#[tokio::test]
async fn given_no_mail_transport_when_a_reset_is_requested_then_it_says_so() {
    // A transport failure is a property of the installation, not of the
    // address, so reporting it reveals nothing — and hiding it would tell an
    // owner their reset is on the way when nothing was sent.
    let tokens = FakeAuthTokenRepository::new();
    let handler = RequestPasswordResetHandler::new(
        credentials(),
        tokens.clone(),
        FailingMailSender,
        clock(),
        AuthMode::Local,
        RESET_TTL_MINUTES,
    );

    let err = handler
        .request(EMAIL)
        .await
        .expect_err("an unconfigured transport must refuse honestly");

    assert_eq!(rejection_code(&err), MAIL_NOT_CONFIGURED);
    assert_eq!(
        tokens.count(),
        0,
        "a token for a message that was never sent must not survive"
    );
}

// ---------------- Complete: the three refusals, and the policy ------------

#[tokio::test]
async fn given_an_unknown_token_when_completed_then_invalid() {
    let credentials = credentials();

    let err = complete_handler(
        credentials.clone(),
        FakeSessionRepository::new(),
        FakeAuthTokenRepository::new(),
        clock(),
    )
    .complete(
        "not-a-token",
        NEW_PASSWORD.to_string(),
        NEW_PASSWORD.to_string(),
    )
    .await
    .expect_err("an unknown token must be refused");

    assert_eq!(rejection_code(&err), RESET_INVALID);
    assert_eq!(
        credentials.get().await.unwrap().unwrap().password_hash,
        "the-old-hash",
        "nothing may change on a refused reset"
    );
}

#[tokio::test]
async fn given_a_spent_token_when_completed_again_then_already_used() {
    let credentials = credentials();
    let tokens = FakeAuthTokenRepository::new();
    let mail = FakeMailSender::new();
    let token = issue_token(&credentials, &tokens, &mail).await;
    let handler = complete_handler(credentials, FakeSessionRepository::new(), tokens, clock());

    handler
        .complete(&token, NEW_PASSWORD.to_string(), NEW_PASSWORD.to_string())
        .await
        .expect("first completion");
    let err = handler
        .complete(&token, NEW_PASSWORD.to_string(), NEW_PASSWORD.to_string())
        .await
        .expect_err("a spent token must be refused");

    assert_eq!(rejection_code(&err), RESET_ALREADY_USED);
}

#[tokio::test]
async fn given_an_expired_token_when_completed_then_expired() {
    let credentials = credentials();
    let tokens = FakeAuthTokenRepository::new();
    let mail = FakeMailSender::new();
    let token = issue_token(&credentials, &tokens, &mail).await;

    let later = clock_at(Duration::minutes(i64::from(RESET_TTL_MINUTES)) + Duration::seconds(1));
    let err = complete_handler(
        credentials.clone(),
        FakeSessionRepository::new(),
        tokens,
        later,
    )
    .complete(&token, NEW_PASSWORD.to_string(), NEW_PASSWORD.to_string())
    .await
    .expect_err("an expired token must be refused");

    assert_eq!(rejection_code(&err), RESET_EXPIRED);
    assert_eq!(
        credentials.get().await.unwrap().unwrap().password_hash,
        "the-old-hash"
    );
}

#[tokio::test]
async fn given_a_weak_new_password_when_completed_then_the_policy_still_applies() {
    // FR-AU-11 holds here exactly as it does at registration — this sets the
    // one password that guards the whole catalog.
    let credentials = credentials();
    let tokens = FakeAuthTokenRepository::new();
    let mail = FakeMailSender::new();
    let token = issue_token(&credentials, &tokens, &mail).await;

    let err = complete_handler(
        credentials.clone(),
        FakeSessionRepository::new(),
        tokens,
        clock(),
    )
    .complete(&token, "short".to_string(), "short".to_string())
    .await
    .expect_err("a weak password must be refused");

    assert_eq!(rejection_code(&err), "password_too_short");
    assert_eq!(
        credentials.get().await.unwrap().unwrap().password_hash,
        "the-old-hash"
    );
}

#[tokio::test]
async fn given_a_password_containing_the_stored_address_when_completed_then_refused() {
    // The request carries no address, so the rule "the password must not
    // contain the address" has to be checked against the stored one.
    let credentials = credentials();
    let tokens = FakeAuthTokenRepository::new();
    let mail = FakeMailSender::new();
    let token = issue_token(&credentials, &tokens, &mail).await;

    let err = complete_handler(credentials, FakeSessionRepository::new(), tokens, clock())
        .complete(
            &token,
            "xxownerxxxxxxxx".to_string(),
            "xxownerxxxxxxxx".to_string(),
        )
        .await
        .expect_err("a password containing the address must be refused");

    assert_eq!(rejection_code(&err), "password_contains_email");
}

#[tokio::test]
async fn given_a_mismatched_confirmation_when_completed_then_refused() {
    let credentials = credentials();
    let tokens = FakeAuthTokenRepository::new();
    let mail = FakeMailSender::new();
    let token = issue_token(&credentials, &tokens, &mail).await;

    let err = complete_handler(credentials, FakeSessionRepository::new(), tokens, clock())
        .complete(
            &token,
            NEW_PASSWORD.to_string(),
            "a quite different passphrases".to_string(),
        )
        .await
        .expect_err("a mismatched confirmation must be refused");

    assert_eq!(rejection_code(&err), "password_confirmation_mismatch");
}

#[tokio::test]
async fn given_a_confirmation_code_when_presented_as_a_reset_token_then_invalid() {
    // The two purposes share a table; a token is only valid for the one it was
    // minted for, however genuine it is.
    let credentials = credentials();
    let tokens = FakeAuthTokenRepository::new();
    let mail = FakeMailSender::new();
    ResendConfirmationHandler::new(
        FakeAuth::Allowing,
        credentials.clone(),
        tokens.clone(),
        mail.clone(),
        clock(),
        AuthMode::Local,
        24,
        60,
    )
    .resend("session")
    .await
    .expect("mint a confirmation code");
    let code = mail.sent().last().expect("a message").secret.clone();

    let err = complete_handler(credentials, FakeSessionRepository::new(), tokens, clock())
        .complete(&code, NEW_PASSWORD.to_string(), NEW_PASSWORD.to_string())
        .await
        .expect_err("a confirmation code is not a reset token");

    assert_eq!(rejection_code(&err), RESET_INVALID);
}
