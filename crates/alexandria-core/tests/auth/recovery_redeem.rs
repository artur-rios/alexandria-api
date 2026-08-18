//! Unit tests for the UC-43 `RedeemRecoveryCodeHandler`: redeem one recovery
//! code for a new password, against trait fakes — no real DB or auth
//! service. Unauthenticated, and invalidates every session on success.

use chrono::{TimeZone, Utc};

use alexandria_core::auth::commands::redeem_recovery_code::RedeemRecoveryCodeHandler;
use alexandria_core::auth::local::{RecoveryCodeRepository, SessionRepository};
use alexandria_core::auth::recovery::{generate_recovery_codes, hash_recovery_code};
use alexandria_core::catalog::clock::FixedClock;
use alexandria_core::config::AuthMode;
use alexandria_core::errors::DomainError;

use crate::common::{
    FakeLocalCredentialRepository, FakeRecoveryCodeRepository, FakeSessionRepository,
};

const NEW_PASSWORD: &str = "correct horse battery";
const EMAIL: &str = "owner@example.com";

fn clock() -> FixedClock {
    FixedClock(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap())
}

type Handler = RedeemRecoveryCodeHandler<
    FakeLocalCredentialRepository,
    FakeSessionRepository,
    FakeRecoveryCodeRepository,
    FixedClock,
>;

/// Seed a credential row and ten recovery codes, then build a handler over
/// the three fakes. Returns the handler, its credential fake, its session
/// fake, its recovery fake, and the ten plaintext codes it seeded — the only
/// place those plaintext codes exist once seeding is done.
async fn handler_with_codes() -> (
    Handler,
    FakeLocalCredentialRepository,
    FakeSessionRepository,
    FakeRecoveryCodeRepository,
    Vec<String>,
) {
    let credentials = FakeLocalCredentialRepository::new();
    credentials.seed(EMAIL, "the-old-hash");

    let sessions = FakeSessionRepository::new();

    let recovery = FakeRecoveryCodeRepository::new();
    let codes = generate_recovery_codes();
    let hashes: Vec<String> = codes.iter().map(|c| hash_recovery_code(c)).collect();
    recovery
        .replace_all(&hashes, clock().0)
        .await
        .expect("seed codes");

    let handler = RedeemRecoveryCodeHandler::new(
        credentials.clone(),
        sessions.clone(),
        recovery.clone(),
        clock(),
        AuthMode::Local,
    );

    (handler, credentials, sessions, recovery, codes)
}

fn handler_without_account() -> Handler {
    RedeemRecoveryCodeHandler::new(
        FakeLocalCredentialRepository::new(),
        FakeSessionRepository::new(),
        FakeRecoveryCodeRepository::new(),
        clock(),
        AuthMode::Local,
    )
}

fn handler_in_external_mode() -> Handler {
    let credentials = FakeLocalCredentialRepository::new();
    credentials.seed(EMAIL, "the-old-hash");
    RedeemRecoveryCodeHandler::new(
        credentials,
        FakeSessionRepository::new(),
        FakeRecoveryCodeRepository::new(),
        clock(),
        AuthMode::External,
    )
}

fn rejection_code(err: &DomainError) -> Option<&str> {
    match err {
        DomainError::Rejected(rejection) => Some(rejection.code),
        _ => None,
    }
}

fn err_is_rejection(err: &DomainError) -> bool {
    matches!(err, DomainError::Rejected(_))
}

#[tokio::test]
async fn given_a_valid_code_when_redeemed_then_password_replaced_and_sessions_cleared() {
    let (handler, credentials, sessions, recovery, codes) = handler_with_codes().await;
    sessions
        .create_session(uuid::Uuid::new_v4(), clock().0, clock().0)
        .await
        .expect("seed a session");
    let before = credentials.stored_hash();

    let result = handler
        .redeem(
            codes[0].clone(),
            NEW_PASSWORD.to_string(),
            NEW_PASSWORD.to_string(),
        )
        .await
        .unwrap();

    assert!(result.success);
    assert_ne!(
        credentials.stored_hash(),
        before,
        "password was not replaced"
    );
    assert!(sessions.all_deleted(), "sessions survived a redemption");
    assert_eq!(recovery.remaining().await.unwrap(), 9);
    assert_eq!(result.recovery_codes_remaining, 9);
}

#[tokio::test]
async fn given_a_code_already_used_when_redeemed_again_then_recovery_code_used() {
    let (handler, credentials, _sessions, _recovery, codes) = handler_with_codes().await;
    handler
        .redeem(
            codes[0].clone(),
            NEW_PASSWORD.to_string(),
            NEW_PASSWORD.to_string(),
        )
        .await
        .unwrap();
    let after_first = credentials.stored_hash();

    let err = handler
        .redeem(
            codes[0].clone(),
            "another good password".to_string(),
            "another good password".to_string(),
        )
        .await
        .unwrap_err();

    assert_eq!(rejection_code(&err), Some("recovery_code_used"));
    assert_eq!(
        credentials.stored_hash(),
        after_first,
        "password changed on a failed redemption"
    );
}

#[tokio::test]
async fn given_a_code_that_was_never_issued_when_redeemed_then_recovery_code_unknown() {
    let (handler, _credentials, _sessions, _recovery, _codes) = handler_with_codes().await;

    let err = handler
        .redeem(
            "MNPQR-STVWX".to_string(),
            NEW_PASSWORD.to_string(),
            NEW_PASSWORD.to_string(),
        )
        .await
        .unwrap_err();

    assert_eq!(rejection_code(&err), Some("recovery_code_unknown"));
}

/// Decision 6: a typo in the new password must not burn a code.
#[tokio::test]
async fn given_a_password_below_the_policy_when_redeemed_then_no_code_is_consumed() {
    let (handler, _credentials, _sessions, recovery, codes) = handler_with_codes().await;

    let err = handler
        .redeem(codes[0].clone(), "short".to_string(), "short".to_string())
        .await
        .unwrap_err();

    assert!(err_is_rejection(&err));
    assert_eq!(
        recovery.remaining().await.unwrap(),
        10,
        "a code was consumed by a bad password"
    );
}

#[tokio::test]
async fn given_a_confirmation_mismatch_when_redeemed_then_no_code_is_consumed() {
    let (handler, _credentials, _sessions, recovery, codes) = handler_with_codes().await;

    let err = handler
        .redeem(
            codes[0].clone(),
            NEW_PASSWORD.to_string(),
            "something else entirely".to_string(),
        )
        .await
        .unwrap_err();

    assert_eq!(rejection_code(&err), Some("password_confirmation_mismatch"));
    assert_eq!(recovery.remaining().await.unwrap(), 10);
}

/// A code is typed off paper; the spelling must not decide whether it works.
#[tokio::test]
async fn given_a_code_typed_lower_case_and_unhyphenated_when_redeemed_then_accepted() {
    let (handler, _credentials, _sessions, recovery, codes) = handler_with_codes().await;
    let typed = codes[0].to_lowercase().replace('-', " ");

    handler
        .redeem(typed, NEW_PASSWORD.to_string(), NEW_PASSWORD.to_string())
        .await
        .unwrap();

    assert_eq!(recovery.remaining().await.unwrap(), 9);
}

#[tokio::test]
async fn given_no_account_when_redeemed_then_not_found() {
    let handler = handler_without_account();

    let err = handler
        .redeem(
            "ABCDE-FGHJK".to_string(),
            NEW_PASSWORD.to_string(),
            NEW_PASSWORD.to_string(),
        )
        .await
        .unwrap_err();

    assert!(matches!(err, DomainError::NotFound));
}

#[tokio::test]
async fn given_external_mode_when_redeemed_then_conflict() {
    let handler = handler_in_external_mode();

    let err = handler
        .redeem(
            "ABCDE-FGHJK".to_string(),
            NEW_PASSWORD.to_string(),
            NEW_PASSWORD.to_string(),
        )
        .await
        .unwrap_err();

    assert!(matches!(err, DomainError::Conflict(_)));
}
