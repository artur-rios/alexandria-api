//! Unit tests for the UC-35 SetLocalCredentialsHandler (Testing
//! Specification §6). Each test exercises exactly the handler against trait
//! fakes — no real DB or auth service. Coverage follows §6.3: happy path for
//! first-time setup and for changing existing credentials, AF-01 (wrong
//! mode), AF-02 (invalid email / empty password), and AF-03 (conditional
//! authorization).

use chrono::{TimeZone, Utc};

use alexandria_core::auth::commands::set_credentials::{
    validate_email, SetLocalCredentialsHandler,
};
use alexandria_core::auth::local::LocalCredentialRepository;
use alexandria_core::auth::password::verify_password;
use alexandria_core::catalog::clock::FixedClock;
use alexandria_core::config::AuthMode;
use alexandria_core::errors::DomainError;

use crate::common::{FakeAuth, FakeLocalCredentialRepository};

const TOKEN: &str = "owner-token";

fn clock() -> FixedClock {
    FixedClock(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap())
}

fn handler(
    auth: FakeAuth,
    repo: FakeLocalCredentialRepository,
    mode: AuthMode,
) -> SetLocalCredentialsHandler<FakeAuth, FakeLocalCredentialRepository, FixedClock> {
    SetLocalCredentialsHandler::new(auth, repo, clock(), mode)
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_first_time_setup_when_set_then_succeeds_without_authenticating() {
    // AF-03: no credentials exist yet, so an unauthenticated (denying) auth
    // service must not block first-time setup.
    let repo = FakeLocalCredentialRepository::new();
    let h = handler(FakeAuth::Denying, repo.clone(), AuthMode::Local);

    let result = h
        .set("owner@example.com".to_string(), "hunter2".to_string(), "")
        .await
        .expect("set");

    assert!(result.success);
    assert_eq!(result.email, "owner@example.com");

    let stored = repo.get().await.unwrap().expect("credential stored");
    assert_eq!(stored.email, "owner@example.com");
    assert!(verify_password("hunter2", &stored.password_hash));
    assert_ne!(
        stored.password_hash, "hunter2",
        "the plaintext must never be stored"
    );
}

#[tokio::test]
async fn given_existing_credentials_and_authenticated_when_set_then_changed() {
    let repo = FakeLocalCredentialRepository::new();
    repo.seed("old@example.com", "irrelevant-hash");
    let h = handler(FakeAuth::Allowing, repo.clone(), AuthMode::Local);

    let result = h
        .set(
            "new@example.com".to_string(),
            "new-password".to_string(),
            TOKEN,
        )
        .await
        .expect("set");

    assert_eq!(result.email, "new@example.com");
    let stored = repo.get().await.unwrap().unwrap();
    assert_eq!(stored.email, "new@example.com");
    assert!(verify_password("new-password", &stored.password_hash));
}

// ---------------- AF-01: wrong mode ----------------

#[tokio::test]
async fn given_external_auth_mode_when_set_then_invalid_state_and_unchanged() {
    let repo = FakeLocalCredentialRepository::new();
    let h = handler(FakeAuth::Allowing, repo.clone(), AuthMode::External);

    let result = h
        .set(
            "owner@example.com".to_string(),
            "hunter2".to_string(),
            TOKEN,
        )
        .await;

    assert!(matches!(result, Err(DomainError::InvalidState)));
    assert!(repo.get().await.unwrap().is_none());
}

// ---------------- AF-03: conditional authorization ----------------

#[tokio::test]
async fn given_existing_credentials_and_unauthenticated_when_set_then_unauthorized_and_unchanged() {
    let repo = FakeLocalCredentialRepository::new();
    repo.seed("old@example.com", "old-hash");
    let h = handler(FakeAuth::Denying, repo.clone(), AuthMode::Local);

    let result = h
        .set(
            "new@example.com".to_string(),
            "new-password".to_string(),
            "",
        )
        .await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
    let stored = repo.get().await.unwrap().unwrap();
    assert_eq!(stored.email, "old@example.com", "unchanged on rejection");
}

// ---------------- AF-02: invalid input ----------------

#[tokio::test]
async fn given_empty_password_when_set_then_invalid_input_and_unchanged() {
    let repo = FakeLocalCredentialRepository::new();
    let h = handler(FakeAuth::Denying, repo.clone(), AuthMode::Local);

    let result = h
        .set("owner@example.com".to_string(), "".to_string(), "")
        .await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert!(repo.get().await.unwrap().is_none());
}

#[tokio::test]
async fn given_invalid_email_when_set_then_invalid_input_and_unchanged() {
    let repo = FakeLocalCredentialRepository::new();
    let h = handler(FakeAuth::Denying, repo.clone(), AuthMode::Local);

    let result = h
        .set("not-an-email".to_string(), "hunter2".to_string(), "")
        .await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    assert!(repo.get().await.unwrap().is_none());
}

#[test]
fn given_various_malformed_emails_when_validated_then_rejected() {
    for bad in [
        "",
        "no-at-sign",
        "@missing-local.com",
        "missing-domain@",
        "two@at@signs.com",
        "no-dot@domain",
        " leading@space.com",
        "trailing@space.com ",
        "dot@.leading.com",
        "dot@trailing.com.",
    ] {
        assert!(
            validate_email(bad).is_err(),
            "expected {bad:?} to be rejected"
        );
    }
}

#[test]
fn given_well_formed_email_when_validated_then_accepted() {
    assert_eq!(
        validate_email("owner@example.com").unwrap(),
        "owner@example.com"
    );
}
