//! Unit tests for the UC-41 RegisterLocalAccountHandler (Testing
//! Specification §6). The handler runs against trait fakes — no real DB,
//! no auth service (registration is unauthenticated by definition).
//! Coverage: the main flow plus AF-01 … AF-06.

use chrono::{TimeZone, Utc};

use alexandria_core::auth::commands::register::RegisterLocalAccountHandler;
use alexandria_core::auth::local::{LocalCredentialRepository, SessionRepository};
use alexandria_core::auth::password::verify_password;
use alexandria_core::catalog::clock::FixedClock;
use alexandria_core::config::AuthMode;
use alexandria_core::errors::DomainError;

use crate::common::{
    FailingSessionRepository, FakeLocalCredentialRepository, FakeSessionRepository,
};

const EMAIL: &str = "owner@example.com";
const PASSWORD: &str = "correct horse battery";
const TTL_HOURS: u32 = 24;

fn clock() -> FixedClock {
    FixedClock(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap())
}

fn handler(
    credentials: FakeLocalCredentialRepository,
    sessions: FakeSessionRepository,
    mode: AuthMode,
) -> RegisterLocalAccountHandler<FakeLocalCredentialRepository, FakeSessionRepository, FixedClock> {
    RegisterLocalAccountHandler::new(credentials, sessions, clock(), mode, TTL_HOURS)
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_no_account_when_register_then_stores_the_hash_and_opens_a_session() {
    let credentials = FakeLocalCredentialRepository::new();
    let sessions = FakeSessionRepository::new();
    let h = handler(credentials.clone(), sessions.clone(), AuthMode::Local);

    let result = h
        .register(
            EMAIL.to_string(),
            PASSWORD.to_string(),
            PASSWORD.to_string(),
        )
        .await
        .expect("register");

    assert!(result.success);
    assert_eq!(result.email, EMAIL);

    let stored = credentials.get().await.unwrap().expect("credential stored");
    assert_eq!(stored.email, EMAIL);
    assert!(verify_password(PASSWORD, &stored.password_hash));
    assert_ne!(
        stored.password_hash, PASSWORD,
        "the plaintext must never be stored"
    );

    assert_eq!(
        sessions.count(),
        1,
        "registration opens exactly one session"
    );
    assert!(
        sessions
            .is_valid(result.session_id, clock().0)
            .await
            .unwrap(),
        "the returned session id must authenticate immediately"
    );
}

// ---------------- AF-01: wrong auth mode ----------------

#[tokio::test]
async fn given_external_auth_mode_when_register_then_conflict_and_nothing_written() {
    let credentials = FakeLocalCredentialRepository::new();
    let sessions = FakeSessionRepository::new();
    let h = handler(credentials.clone(), sessions.clone(), AuthMode::External);

    let err = h
        .register(
            EMAIL.to_string(),
            PASSWORD.to_string(),
            PASSWORD.to_string(),
        )
        .await
        .expect_err("must reject in external mode");

    assert!(matches!(err, DomainError::Conflict(_)), "got {err:?}");
    assert!(credentials.get().await.unwrap().is_none());
    assert_eq!(sessions.count(), 0);
}

// ---------------- AF-02: account already exists ----------------

#[tokio::test]
async fn given_an_existing_account_when_register_then_conflict_and_credentials_untouched() {
    let credentials = FakeLocalCredentialRepository::new();
    credentials
        .upsert(EMAIL, "existing-hash", clock().0)
        .await
        .unwrap();
    let sessions = FakeSessionRepository::new();
    let h = handler(credentials.clone(), sessions.clone(), AuthMode::Local);

    let err = h
        .register(
            "someone-else@example.com".to_string(),
            PASSWORD.to_string(),
            PASSWORD.to_string(),
        )
        .await
        .expect_err("must reject a second registration");

    assert!(matches!(err, DomainError::Conflict(_)), "got {err:?}");
    let stored = credentials.get().await.unwrap().expect("credential");
    assert_eq!(stored.email, EMAIL, "the stored email must be untouched");
    assert_eq!(stored.password_hash, "existing-hash");
    assert_eq!(sessions.count(), 0);
}

#[tokio::test]
async fn given_an_existing_account_and_a_weak_password_when_register_then_conflict_wins() {
    // Ordering matters: existence is checked before the input rules, so a
    // caller cannot probe stored state by varying the password.
    let credentials = FakeLocalCredentialRepository::new();
    credentials
        .upsert(EMAIL, "existing-hash", clock().0)
        .await
        .unwrap();
    let h = handler(credentials, FakeSessionRepository::new(), AuthMode::Local);

    let err = h
        .register(EMAIL.to_string(), "short".to_string(), "short".to_string())
        .await
        .expect_err("must reject");

    assert!(matches!(err, DomainError::Conflict(_)), "got {err:?}");
}

// ---------------- AF-03: invalid email ----------------

#[tokio::test]
async fn given_a_malformed_email_when_register_then_invalid_input_and_nothing_written() {
    let credentials = FakeLocalCredentialRepository::new();
    let sessions = FakeSessionRepository::new();
    let h = handler(credentials.clone(), sessions.clone(), AuthMode::Local);

    let err = h
        .register(
            "not-an-email".to_string(),
            PASSWORD.to_string(),
            PASSWORD.to_string(),
        )
        .await
        .expect_err("must reject a malformed email");

    assert!(matches!(err, DomainError::InvalidInput(_)), "got {err:?}");
    assert!(credentials.get().await.unwrap().is_none());
    assert_eq!(sessions.count(), 0);
}

// ---------------- AF-04: weak password ----------------

#[tokio::test]
async fn given_a_password_below_the_length_floor_when_register_then_invalid_input() {
    let credentials = FakeLocalCredentialRepository::new();
    let sessions = FakeSessionRepository::new();
    let h = handler(credentials.clone(), sessions.clone(), AuthMode::Local);

    let err = h
        .register(EMAIL.to_string(), "short".to_string(), "short".to_string())
        .await
        .expect_err("must reject a weak password");

    match err {
        DomainError::InvalidInput(message) => {
            assert!(message.contains("at least"), "unexpected: {message}");
            assert!(!message.contains("short"), "must not echo the password");
        }
        other => panic!("expected InvalidInput, got {other:?}"),
    }
    assert!(credentials.get().await.unwrap().is_none());
    assert_eq!(sessions.count(), 0);
}

// ---------------- AF-05: confirmation mismatch ----------------

#[tokio::test]
async fn given_a_mismatched_confirmation_when_register_then_invalid_input_and_nothing_written() {
    let credentials = FakeLocalCredentialRepository::new();
    let sessions = FakeSessionRepository::new();
    let h = handler(credentials.clone(), sessions.clone(), AuthMode::Local);

    let err = h
        .register(
            EMAIL.to_string(),
            PASSWORD.to_string(),
            "correct horse batteries".to_string(),
        )
        .await
        .expect_err("must reject a mismatched confirmation");

    assert!(matches!(err, DomainError::InvalidInput(_)), "got {err:?}");
    assert!(credentials.get().await.unwrap().is_none());
    assert_eq!(sessions.count(), 0);
}

// ---------------- AF-06: session creation fails after the write ----------------

#[tokio::test]
async fn given_session_creation_fails_when_register_then_errors_but_the_account_survives() {
    let credentials = FakeLocalCredentialRepository::new();
    let h = RegisterLocalAccountHandler::new(
        credentials.clone(),
        FailingSessionRepository,
        clock(),
        AuthMode::Local,
        TTL_HOURS,
    );

    let err = h
        .register(
            EMAIL.to_string(),
            PASSWORD.to_string(),
            PASSWORD.to_string(),
        )
        .await
        .expect_err("the session failure must surface");

    assert!(matches!(err, DomainError::Disk(_)), "got {err:?}");
    let stored = credentials.get().await.unwrap().expect("credential stored");
    assert_eq!(
        stored.email, EMAIL,
        "AF-06: the account exists; the caller obtains a session via UC-34"
    );
}
