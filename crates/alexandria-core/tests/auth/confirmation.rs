//! Unit tests for e-mail confirmation — the confirm and resend commands
//! (issue #102 / FR-AU-13 … FR-AU-15), against trait fakes with no database.
//!
//! Confirmation depends on a code that only the outbound message carries, so
//! these tests mint one through the resend command and read it back off the
//! fake mail sender — the same path a real owner takes, rather than reaching
//! into storage for a value the API never hands out.

use chrono::{Duration, TimeZone, Utc};

use alexandria_core::auth::commands::account_status::GetLocalAccountHandler;
use alexandria_core::auth::commands::confirm_email::{
    ConfirmEmailHandler, CONFIRMATION_ALREADY_USED, CONFIRMATION_EXPIRED, CONFIRMATION_INVALID,
};
use alexandria_core::auth::commands::resend_confirmation::{
    ResendConfirmationHandler, RESEND_TOO_SOON,
};
use alexandria_core::auth::local::LocalCredentialRepository;
use alexandria_core::auth::mail::{MailKind, MAIL_NOT_CONFIGURED};
use alexandria_core::auth::tokens::hash_token;
use alexandria_core::catalog::clock::FixedClock;
use alexandria_core::config::AuthMode;
use alexandria_core::errors::DomainError;

use crate::common::{
    FailingMailSender, FakeAuth, FakeAuthTokenRepository, FakeLocalCredentialRepository,
    FakeMailSender,
};

const EMAIL: &str = "owner@example.com";
const CONFIRMATION_TTL_HOURS: u32 = 24;
const RESEND_INTERVAL_SECONDS: u32 = 60;

fn clock_at(offset: Duration) -> FixedClock {
    FixedClock(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap() + offset)
}

fn clock() -> FixedClock {
    clock_at(Duration::zero())
}

fn seeded_credentials() -> FakeLocalCredentialRepository {
    let repo = FakeLocalCredentialRepository::new();
    repo.seed(EMAIL, "irrelevant-hash");
    repo
}

fn resend_handler(
    credentials: FakeLocalCredentialRepository,
    tokens: FakeAuthTokenRepository,
    mail: FakeMailSender,
    clock: FixedClock,
) -> ResendConfirmationHandler<
    FakeAuth,
    FakeLocalCredentialRepository,
    FakeAuthTokenRepository,
    FakeMailSender,
    FixedClock,
> {
    ResendConfirmationHandler::new(
        FakeAuth::Allowing,
        credentials,
        tokens,
        mail,
        clock,
        AuthMode::Local,
        CONFIRMATION_TTL_HOURS,
        RESEND_INTERVAL_SECONDS,
    )
}

fn confirm_handler(
    credentials: FakeLocalCredentialRepository,
    tokens: FakeAuthTokenRepository,
    clock: FixedClock,
) -> ConfirmEmailHandler<FakeLocalCredentialRepository, FakeAuthTokenRepository, FixedClock> {
    ConfirmEmailHandler::new(credentials, tokens, clock, AuthMode::Local)
}

/// Mint a confirmation code the way an owner receives one: through resend,
/// read off the message that was "delivered".
async fn issue_code(
    credentials: &FakeLocalCredentialRepository,
    tokens: &FakeAuthTokenRepository,
    mail: &FakeMailSender,
    clock: FixedClock,
) -> String {
    resend_handler(credentials.clone(), tokens.clone(), mail.clone(), clock)
        .resend("session")
        .await
        .expect("resend must succeed with a working transport");
    let sent = mail.sent();
    let message = sent.last().expect("a message must have been sent");
    assert_eq!(message.kind, MailKind::EmailConfirmation);
    assert_eq!(message.to, EMAIL);
    message.secret.clone()
}

fn rejection_code(err: &DomainError) -> &str {
    match err {
        DomainError::Rejected(rejection) | DomainError::TooManyRequests(rejection) => {
            rejection.code
        }
        DomainError::Unavailable(rejection) => rejection.code,
        other => panic!("expected a rejection, got {other:?}"),
    }
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_a_valid_code_when_confirmed_then_the_address_is_confirmed() {
    let credentials = seeded_credentials();
    let tokens = FakeAuthTokenRepository::new();
    let mail = FakeMailSender::new();
    let code = issue_code(&credentials, &tokens, &mail, clock()).await;

    let result = confirm_handler(credentials.clone(), tokens.clone(), clock())
        .confirm(&code)
        .await
        .expect("a freshly issued code must confirm");

    assert!(result.success);
    assert!(result.email_confirmed);
    assert_eq!(result.email, EMAIL);
    assert!(credentials.get().await.unwrap().unwrap().email_confirmed());
}

#[tokio::test]
async fn given_a_confirmed_account_when_the_state_is_read_then_it_reports_confirmed() {
    // FR-AU-13: this is the query the front-end's catalog lock reads.
    let credentials = seeded_credentials();
    let tokens = FakeAuthTokenRepository::new();
    let mail = FakeMailSender::new();
    let account = GetLocalAccountHandler::new(FakeAuth::Allowing, credentials.clone());

    let before = account.get("session").await.expect("account state");
    assert_eq!(before.email, EMAIL);
    assert!(!before.email_confirmed, "a new account is unconfirmed");

    let code = issue_code(&credentials, &tokens, &mail, clock()).await;
    confirm_handler(credentials.clone(), tokens, clock())
        .confirm(&code)
        .await
        .expect("confirm");

    assert!(
        account
            .get("session")
            .await
            .expect("account state")
            .email_confirmed
    );
}

#[tokio::test]
async fn given_a_code_when_stored_then_only_its_hash_is_written() {
    // FR-AU-19: a database read must not yield a working code.
    let credentials = seeded_credentials();
    let tokens = FakeAuthTokenRepository::new();
    let mail = FakeMailSender::new();
    let code = issue_code(&credentials, &tokens, &mail, clock()).await;

    let stored = tokens.stored_hashes();
    assert!(
        !stored.iter().any(|value| value == &code),
        "the plaintext code must never be stored"
    );
    assert!(
        stored.contains(&hash_token(&code)),
        "the code's hash must be what was stored"
    );
}

// ---------------- Confirm: the three refusals ----------------

#[tokio::test]
async fn given_an_unknown_code_when_confirmed_then_invalid() {
    let credentials = seeded_credentials();
    let tokens = FakeAuthTokenRepository::new();

    let err = confirm_handler(credentials.clone(), tokens, clock())
        .confirm("NOTACODE")
        .await
        .expect_err("an unknown code must be refused");

    assert_eq!(rejection_code(&err), CONFIRMATION_INVALID);
    assert!(!credentials.get().await.unwrap().unwrap().email_confirmed());
}

#[tokio::test]
async fn given_a_spent_code_when_confirmed_again_on_an_unconfirmed_account_then_already_used() {
    // Reached by confirming, then un-confirming is impossible — so this drives
    // it through a second, *different* account state: the code is consumed by
    // the first confirm, and a fresh handler over the same tokens but an
    // unconfirmed credential row sees it as spent.
    let credentials = seeded_credentials();
    let tokens = FakeAuthTokenRepository::new();
    let mail = FakeMailSender::new();
    let code = issue_code(&credentials, &tokens, &mail, clock()).await;

    confirm_handler(credentials.clone(), tokens.clone(), clock())
        .confirm(&code)
        .await
        .expect("first confirm");

    let unconfirmed = seeded_credentials();
    let err = confirm_handler(unconfirmed, tokens, clock())
        .confirm(&code)
        .await
        .expect_err("a spent code must be refused");

    assert_eq!(rejection_code(&err), CONFIRMATION_ALREADY_USED);
}

#[tokio::test]
async fn given_an_expired_code_when_confirmed_then_expired() {
    let credentials = seeded_credentials();
    let tokens = FakeAuthTokenRepository::new();
    let mail = FakeMailSender::new();
    let code = issue_code(&credentials, &tokens, &mail, clock()).await;

    // One second past the lifetime.
    let later = clock_at(Duration::hours(i64::from(CONFIRMATION_TTL_HOURS)) + Duration::seconds(1));
    let err = confirm_handler(credentials.clone(), tokens, later)
        .confirm(&code)
        .await
        .expect_err("an expired code must be refused");

    assert_eq!(rejection_code(&err), CONFIRMATION_EXPIRED);
    assert!(!credentials.get().await.unwrap().unwrap().email_confirmed());
}

#[tokio::test]
async fn given_an_already_confirmed_account_when_the_same_code_is_presented_then_idempotent() {
    // A retry, or a second tap on the same button, must not read as a failure:
    // what the owner asked for has already happened.
    let credentials = seeded_credentials();
    let tokens = FakeAuthTokenRepository::new();
    let mail = FakeMailSender::new();
    let code = issue_code(&credentials, &tokens, &mail, clock()).await;
    let handler = confirm_handler(credentials.clone(), tokens, clock());

    handler.confirm(&code).await.expect("first confirm");
    let again = handler.confirm(&code).await.expect("second confirm");

    assert!(again.success);
    assert!(again.email_confirmed);
}

#[tokio::test]
async fn given_an_already_confirmed_account_when_an_unknown_code_is_presented_then_invalid() {
    // Idempotence is not a reason to accept anything at all.
    let credentials = seeded_credentials();
    let tokens = FakeAuthTokenRepository::new();
    let mail = FakeMailSender::new();
    let code = issue_code(&credentials, &tokens, &mail, clock()).await;
    let handler = confirm_handler(credentials.clone(), tokens, clock());
    handler.confirm(&code).await.expect("first confirm");

    let err = handler
        .confirm("NOTACODE")
        .await
        .expect_err("an unknown code must still be refused");

    assert_eq!(rejection_code(&err), CONFIRMATION_INVALID);
}

#[tokio::test]
async fn given_an_earlier_code_when_a_later_one_confirms_then_the_earlier_stops_working() {
    let credentials = seeded_credentials();
    let tokens = FakeAuthTokenRepository::new();
    let mail = FakeMailSender::new();
    let first = issue_code(&credentials, &tokens, &mail, clock()).await;
    let second = issue_code(
        &credentials,
        &tokens,
        &mail,
        clock_at(Duration::seconds(i64::from(RESEND_INTERVAL_SECONDS))),
    )
    .await;
    assert_ne!(first, second);

    confirm_handler(credentials.clone(), tokens.clone(), clock())
        .confirm(&second)
        .await
        .expect("the newest code confirms");

    // The earlier code is spent too — proved against an unconfirmed account,
    // where the idempotent path cannot mask it.
    let err = confirm_handler(seeded_credentials(), tokens, clock())
        .confirm(&first)
        .await
        .expect_err("an earlier code must stop working");
    assert_eq!(rejection_code(&err), CONFIRMATION_ALREADY_USED);
}

// ---------------- Resend ----------------

#[tokio::test]
async fn given_a_resend_inside_the_interval_when_requested_then_too_soon_with_the_wait() {
    let credentials = seeded_credentials();
    let tokens = FakeAuthTokenRepository::new();
    let mail = FakeMailSender::new();
    issue_code(&credentials, &tokens, &mail, clock()).await;

    let ten_seconds_later = clock_at(Duration::seconds(10));
    let err = resend_handler(credentials, tokens, mail.clone(), ten_seconds_later)
        .resend("session")
        .await
        .expect_err("a resend inside the interval must be refused");

    assert_eq!(rejection_code(&err), RESEND_TOO_SOON);
    let DomainError::TooManyRequests(rejection) = &err else {
        panic!("expected TooManyRequests, got {err:?}");
    };
    assert_eq!(
        rejection
            .params
            .get("retryAfterSeconds")
            .map(String::as_str),
        Some("50"),
        "the wait must be what is actually left of the interval"
    );
    assert_eq!(mail.sent().len(), 1, "nothing further may be sent");
}

#[tokio::test]
async fn given_a_resend_after_the_interval_when_requested_then_a_new_code_is_sent() {
    let credentials = seeded_credentials();
    let tokens = FakeAuthTokenRepository::new();
    let mail = FakeMailSender::new();
    issue_code(&credentials, &tokens, &mail, clock()).await;

    let after = clock_at(Duration::seconds(i64::from(RESEND_INTERVAL_SECONDS)));
    resend_handler(credentials, tokens, mail.clone(), after)
        .resend("session")
        .await
        .expect("a resend after the interval must be allowed");

    assert_eq!(mail.sent().len(), 2);
}

#[tokio::test]
async fn given_a_confirmed_account_when_resend_requested_then_conflict() {
    let credentials = seeded_credentials();
    let tokens = FakeAuthTokenRepository::new();
    let mail = FakeMailSender::new();
    let code = issue_code(&credentials, &tokens, &mail, clock()).await;
    confirm_handler(credentials.clone(), tokens.clone(), clock())
        .confirm(&code)
        .await
        .expect("confirm");

    let after = clock_at(Duration::hours(1));
    let err = resend_handler(credentials, tokens, mail, after)
        .resend("session")
        .await
        .expect_err("resending on a confirmed account must be refused");

    assert!(matches!(err, DomainError::Conflict(_)), "got {err:?}");
}

#[tokio::test]
async fn given_an_unauthenticated_caller_when_resend_requested_then_unauthorized() {
    let handler = ResendConfirmationHandler::new(
        FakeAuth::Denying,
        seeded_credentials(),
        FakeAuthTokenRepository::new(),
        FakeMailSender::new(),
        clock(),
        AuthMode::Local,
        CONFIRMATION_TTL_HOURS,
        RESEND_INTERVAL_SECONDS,
    );

    let err = handler
        .resend("session")
        .await
        .expect_err("resend must authenticate");

    assert!(matches!(err, DomainError::Unauthorized), "got {err:?}");
}

// ---------------- The unconfigured transport (today's every install) --------

#[tokio::test]
async fn given_no_mail_transport_when_resend_requested_then_it_says_so_and_stores_nothing() {
    let credentials = seeded_credentials();
    let tokens = FakeAuthTokenRepository::new();
    let handler = ResendConfirmationHandler::new(
        FakeAuth::Allowing,
        credentials,
        tokens.clone(),
        FailingMailSender,
        clock(),
        AuthMode::Local,
        CONFIRMATION_TTL_HOURS,
        RESEND_INTERVAL_SECONDS,
    );

    let err = handler
        .resend("session")
        .await
        .expect_err("an unconfigured transport must refuse honestly");

    assert_eq!(rejection_code(&err), MAIL_NOT_CONFIGURED);
    // The code never left the building, so it must leave no trace: not a
    // usable code, and not a row that makes the next attempt look too soon.
    assert_eq!(
        tokens.count(),
        0,
        "a token for a message that was never sent must not survive"
    );
}

#[tokio::test]
async fn given_a_failed_send_when_resend_is_retried_immediately_then_it_is_not_rate_limited() {
    // The point of the previous test's cleanup: a caller must see the real
    // reason on every attempt, not a wait that protects nothing.
    let handler = ResendConfirmationHandler::new(
        FakeAuth::Allowing,
        seeded_credentials(),
        FakeAuthTokenRepository::new(),
        FailingMailSender,
        clock(),
        AuthMode::Local,
        CONFIRMATION_TTL_HOURS,
        RESEND_INTERVAL_SECONDS,
    );

    handler.resend("session").await.expect_err("first attempt");
    let err = handler.resend("session").await.expect_err("second attempt");

    assert_eq!(rejection_code(&err), MAIL_NOT_CONFIGURED);
}
