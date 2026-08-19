//! Unit tests for the UC-45 WindowsLoginHandler (Testing Specification §6).
//! The handler takes no credentials — the process's account was already
//! verified against the configured SID at startup — so coverage is: happy
//! path (a session is opened and is actually valid afterwards) and AF-01
//! (wrong active auth mode).

use alexandria_core::auth::commands::windows_login::WindowsLoginHandler;
use alexandria_core::auth::local::SessionRepository;
use alexandria_core::catalog::clock::FixedClock;
use alexandria_core::config::AuthMode;
use alexandria_core::errors::DomainError;

use crate::common::FakeSessionRepository;

const TTL_HOURS: u32 = 24;

fn handler(mode: AuthMode) -> WindowsLoginHandler<FakeSessionRepository, FixedClock> {
    WindowsLoginHandler::new(FakeSessionRepository::new(), clock(), mode, TTL_HOURS)
}

/// Reuse `login.rs`'s fixed clock construction verbatim.
fn clock() -> FixedClock {
    use chrono::{TimeZone, Utc};
    FixedClock(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap())
}

#[tokio::test]
async fn given_windows_mode_when_logged_in_then_a_session_is_opened() {
    let handler = handler(AuthMode::Windows);

    let result = handler.login().await.unwrap();

    assert!(result.success);
    assert!(!result.session_id.is_nil());
}

/// The session must be usable afterwards — minting an id that was never
/// stored would pass a shallower assertion.
#[tokio::test]
async fn given_a_session_from_windows_login_when_checked_then_it_is_valid() {
    let sessions = FakeSessionRepository::new();
    let handler = WindowsLoginHandler::new(sessions.clone(), clock(), AuthMode::Windows, TTL_HOURS);

    let result = handler.login().await.unwrap();

    assert!(sessions
        .is_valid(result.session_id, clock().0)
        .await
        .unwrap());
}

#[tokio::test]
async fn given_local_mode_when_windows_login_attempted_then_conflict() {
    let err = handler(AuthMode::Local).login().await.unwrap_err();

    assert!(matches!(err, DomainError::Conflict(_)));
}

#[tokio::test]
async fn given_external_mode_when_windows_login_attempted_then_conflict() {
    let err = handler(AuthMode::External).login().await.unwrap_err();

    assert!(matches!(err, DomainError::Conflict(_)));
}
