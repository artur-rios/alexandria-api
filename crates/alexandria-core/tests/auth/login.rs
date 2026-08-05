//! Unit tests for the UC-34 LocalLoginHandler (Testing Specification §6).
//! Each test exercises exactly the handler against trait fakes — no real DB.
//! Coverage follows §6.3: happy path (a session is created), AF-01 (wrong
//! mode), AF-02 (wrong email/password), and AF-03 (no credentials set yet).

use chrono::{TimeZone, Utc};

use alexandria_core::auth::commands::login::LocalLoginHandler;
use alexandria_core::auth::local::SessionRepository;
use alexandria_core::auth::password::hash_password;
use alexandria_core::catalog::clock::FixedClock;
use alexandria_core::config::AuthMode;
use alexandria_core::errors::DomainError;

use crate::common::{FakeLocalCredentialRepository, FakeSessionRepository};

const SESSION_TTL_HOURS: u32 = 24;

fn clock() -> FixedClock {
    FixedClock(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap())
}

fn handler(
    credentials: FakeLocalCredentialRepository,
    sessions: FakeSessionRepository,
    mode: AuthMode,
) -> LocalLoginHandler<FakeLocalCredentialRepository, FakeSessionRepository, FixedClock> {
    LocalLoginHandler::new(credentials, sessions, clock(), mode, SESSION_TTL_HOURS)
}

fn seeded_repo(email: &str, password: &str) -> FakeLocalCredentialRepository {
    let repo = FakeLocalCredentialRepository::new();
    repo.seed(email, &hash_password(password).unwrap());
    repo
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_correct_credentials_when_login_then_session_created_and_returned() {
    let credentials = seeded_repo("owner@example.com", "hunter2");
    let sessions = FakeSessionRepository::new();
    let h = handler(credentials, sessions.clone(), AuthMode::Local);

    let result = h
        .login("owner@example.com", "hunter2")
        .await
        .expect("login");

    assert!(result.success);
    assert_eq!(sessions.count(), 1);
    assert!(sessions
        .is_valid(
            result.session_id,
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 1).unwrap()
        )
        .await
        .unwrap());
}

// ---------------- AF-01: wrong mode ----------------

#[tokio::test]
async fn given_external_auth_mode_when_login_then_unauthorized_and_no_session() {
    let credentials = seeded_repo("owner@example.com", "hunter2");
    let sessions = FakeSessionRepository::new();
    let h = handler(credentials, sessions.clone(), AuthMode::External);

    let result = h.login("owner@example.com", "hunter2").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
    assert_eq!(sessions.count(), 0);
}

// ---------------- AF-03: credentials not set ----------------

#[tokio::test]
async fn given_no_credentials_set_when_login_then_config_error_and_no_session() {
    let credentials = FakeLocalCredentialRepository::new();
    let sessions = FakeSessionRepository::new();
    let h = handler(credentials, sessions.clone(), AuthMode::Local);

    let result = h.login("owner@example.com", "hunter2").await;

    assert!(matches!(result, Err(DomainError::Config(_))));
    assert_eq!(sessions.count(), 0);
}

// ---------------- AF-02: wrong email or password ----------------

#[tokio::test]
async fn given_wrong_password_when_login_then_unauthorized_and_no_session() {
    let credentials = seeded_repo("owner@example.com", "hunter2");
    let sessions = FakeSessionRepository::new();
    let h = handler(credentials, sessions.clone(), AuthMode::Local);

    let result = h.login("owner@example.com", "wrong").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
    assert_eq!(sessions.count(), 0);
}

#[tokio::test]
async fn given_wrong_email_when_login_then_unauthorized_and_no_session() {
    let credentials = seeded_repo("owner@example.com", "hunter2");
    let sessions = FakeSessionRepository::new();
    let h = handler(credentials, sessions.clone(), AuthMode::Local);

    let result = h.login("someone-else@example.com", "hunter2").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
    assert_eq!(sessions.count(), 0);
}
