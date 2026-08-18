//! Unit tests for the `GetLocalAccountHandler` (FR-AU-13, FR-AU-18). The
//! handler runs against trait fakes — no real DB.

use chrono::Utc;

use alexandria_core::auth::commands::account_status::GetLocalAccountHandler;
use alexandria_core::auth::local::RecoveryCodeRepository;

use crate::common::{FakeAuth, FakeLocalCredentialRepository, FakeRecoveryCodeRepository};

type TestAccountHandler =
    GetLocalAccountHandler<FakeAuth, FakeLocalCredentialRepository, FakeRecoveryCodeRepository>;

/// A handler over an already-authenticated owner with a seeded credential
/// row, and a fresh recovery-code fake the test can seed itself.
fn handler_with_recovery() -> (TestAccountHandler, FakeRecoveryCodeRepository) {
    let credentials = FakeLocalCredentialRepository::new();
    credentials.seed("owner@example.com", "hash");
    let recovery = FakeRecoveryCodeRepository::new();
    let handler = GetLocalAccountHandler::new(FakeAuth::Allowing, credentials, recovery.clone());
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
