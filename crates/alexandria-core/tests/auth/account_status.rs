//! Unit tests for the `GetLocalAccountHandler` (FR-AU-18). The handler runs
//! against trait fakes — no real DB.

use chrono::Utc;

use alexandria_core::auth::commands::account_status::GetLocalAccountHandler;
use alexandria_core::auth::local::RecoveryCodeRepository;
use alexandria_core::config::AuthMode;
use alexandria_core::errors::DomainError;

use crate::common::{FakeAuth, FakeLocalCredentialRepository, FakeRecoveryCodeRepository};

type TestAccountHandler =
    GetLocalAccountHandler<FakeAuth, FakeLocalCredentialRepository, FakeRecoveryCodeRepository>;

/// A handler over an already-authenticated owner with a seeded credential
/// row, and a fresh recovery-code fake the test can seed itself.
fn handler_with_recovery() -> (TestAccountHandler, FakeRecoveryCodeRepository) {
    handler_in_mode(AuthMode::Local)
}

/// The same handler under an arbitrary active auth mode, so the non-local
/// refusal can be exercised against an otherwise perfectly valid account.
fn handler_in_mode(mode: AuthMode) -> (TestAccountHandler, FakeRecoveryCodeRepository) {
    let credentials = FakeLocalCredentialRepository::new();
    credentials.seed("owner@example.com", "hash");
    let recovery = FakeRecoveryCodeRepository::new();
    let handler =
        GetLocalAccountHandler::new(FakeAuth::Allowing, credentials, recovery.clone(), mode);
    (handler, recovery)
}

#[tokio::test]
async fn given_codes_remaining_when_account_is_read_then_the_count_is_reported() {
    let (handler, recovery) = handler_with_recovery();
    recovery
        .replace_all(&["a".to_string(), "b".to_string()], Utc::now())
        .await
        .unwrap();

    let result = handler.get("session").await.unwrap();

    assert_eq!(result.recovery_codes_remaining, 2);
}

/// An account registered before recovery codes existed holds none, and the
/// count is how its owner learns to regenerate.
#[tokio::test]
async fn given_an_account_with_no_codes_when_read_then_zero_is_reported() {
    let (handler, _recovery) = handler_with_recovery();

    assert_eq!(
        handler
            .get("session")
            .await
            .unwrap()
            .recovery_codes_remaining,
        0
    );
}

/// FR-AU-23 / design decision 5: in Windows mode there is no stored local
/// credential, so `account` refuses like its four siblings rather than
/// authenticating and then failing to find a row.
#[tokio::test]
async fn given_windows_mode_when_account_is_read_then_it_is_refused() {
    let (handler, _recovery) = handler_in_mode(AuthMode::Windows);

    let error = handler.get("session").await.unwrap_err();

    assert!(
        matches!(&error, DomainError::Conflict(message) if message.contains("active auth mode")),
        "{error:?}"
    );
}

/// The same refusal in external mode, for the same reason: FR-AU-03 makes
/// every local-mode operation an invalid operation while another mode is
/// active, and `account` is one of them.
#[tokio::test]
async fn given_external_mode_when_account_is_read_then_it_is_refused() {
    let (handler, _recovery) = handler_in_mode(AuthMode::External);

    assert!(matches!(
        handler.get("session").await.unwrap_err(),
        DomainError::Conflict(_)
    ));
}
