//! Unit tests for the UC-44 `RegenerateRecoveryCodesHandler`: issue a fresh
//! set of recovery codes, invalidating every old one, against trait fakes —
//! no real DB or auth service. Modeled on `tests/auth/set_credentials.rs`,
//! the closest authenticated-command analogue.

use chrono::{TimeZone, Utc};

use alexandria_core::auth::commands::regenerate_recovery_codes::RegenerateRecoveryCodesHandler;
use alexandria_core::auth::local::{RecoveryCodeOutcome, RecoveryCodeRepository};
use alexandria_core::auth::recovery::{generate_recovery_codes, hash_recovery_code};
use alexandria_core::catalog::clock::FixedClock;
use alexandria_core::config::AuthMode;
use alexandria_core::errors::DomainError;

use crate::common::{FakeAuth, FakeLocalCredentialRepository, FakeRecoveryCodeRepository};

const EMAIL: &str = "owner@example.com";

fn clock() -> FixedClock {
    FixedClock(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap())
}

type Handler = RegenerateRecoveryCodesHandler<
    FakeAuth,
    FakeLocalCredentialRepository,
    FakeRecoveryCodeRepository,
    FixedClock,
>;

/// Seed a credential row and ten recovery codes, then build an authenticated
/// handler over the two fakes. Returns the handler, its recovery fake, and
/// the ten plaintext codes it seeded.
async fn handler_with_codes() -> (Handler, FakeRecoveryCodeRepository, Vec<String>) {
    let credentials = FakeLocalCredentialRepository::new();
    credentials.seed(EMAIL, "the-old-hash");

    let recovery = FakeRecoveryCodeRepository::new();
    let codes = generate_recovery_codes();
    let hashes: Vec<String> = codes.iter().map(|c| hash_recovery_code(c)).collect();
    recovery
        .replace_all(&hashes, clock().0)
        .await
        .expect("seed codes");

    let handler = RegenerateRecoveryCodesHandler::new(
        FakeAuth::Allowing,
        credentials,
        recovery.clone(),
        clock(),
        AuthMode::Local,
    );

    (handler, recovery, codes)
}

fn handler_rejecting_auth() -> Handler {
    let credentials = FakeLocalCredentialRepository::new();
    credentials.seed(EMAIL, "the-old-hash");
    RegenerateRecoveryCodesHandler::new(
        FakeAuth::Denying,
        credentials,
        FakeRecoveryCodeRepository::new(),
        clock(),
        AuthMode::Local,
    )
}

fn handler_in_external_mode() -> Handler {
    let credentials = FakeLocalCredentialRepository::new();
    credentials.seed(EMAIL, "the-old-hash");
    RegenerateRecoveryCodesHandler::new(
        FakeAuth::Allowing,
        credentials,
        FakeRecoveryCodeRepository::new(),
        clock(),
        AuthMode::External,
    )
}

#[tokio::test]
async fn given_an_authenticated_owner_when_regenerated_then_ten_new_codes() {
    let (handler, recovery, old_codes) = handler_with_codes().await;

    let result = handler.regenerate("session").await.unwrap();

    assert_eq!(result.recovery_codes.len(), 10);
    assert_eq!(recovery.remaining().await.unwrap(), 10);
    for old in &old_codes {
        assert!(!result.recovery_codes.contains(old));
    }
}

/// FR-AU-17: an unused code from the old set must stop working, or the
/// owner cannot tell which of their written codes are live.
#[tokio::test]
async fn given_unused_old_codes_when_regenerated_then_they_no_longer_work() {
    let (handler, recovery, old_codes) = handler_with_codes().await;

    handler.regenerate("session").await.unwrap();

    assert_eq!(
        recovery
            .consume(&hash_recovery_code(&old_codes[7]), Utc::now())
            .await
            .unwrap(),
        RecoveryCodeOutcome::Unknown
    );
}

#[tokio::test]
async fn given_an_unauthenticated_caller_when_regenerating_then_unauthorized() {
    let handler = handler_rejecting_auth();

    let err = handler.regenerate("nonsense").await.unwrap_err();

    assert!(matches!(err, DomainError::Unauthorized));
}

#[tokio::test]
async fn given_external_mode_when_regenerating_then_conflict() {
    let handler = handler_in_external_mode();

    let err = handler.regenerate("session").await.unwrap_err();

    assert!(matches!(err, DomainError::Conflict(_)));
}
