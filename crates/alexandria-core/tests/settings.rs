//! Unit tests for the UC-47 GetSettingsHandler (Testing Specification §6).
//! The handler against a trait fake — no database, no auth service. Coverage
//! follows §6.3: the happy path, the configured value rather than the default,
//! and AF-01 (unauthorized).

#[path = "common/mod.rs"]
mod common;

use alexandria_core::config::Settings;
use alexandria_core::errors::DomainError;
use alexandria_core::settings::GetSettingsHandler;

use crate::common::FakeAuth;

const TOKEN: &str = "bearer-token";

fn settings_with(retention_days: u32) -> Settings {
    let mut settings = Settings::default();
    settings.deletion.retention_days = retention_days;
    settings
}

#[tokio::test]
async fn given_the_default_settings_when_read_then_the_default_window_is_reported() {
    let handler = GetSettingsHandler::new(FakeAuth::Allowing, &Settings::default());

    let view = handler.get(TOKEN).await.unwrap();

    assert_eq!(view.deletion.retention_days, 30);
}

/// The point of the use case: a client must read what this server enforces,
/// not what the default happens to be.
#[tokio::test]
async fn given_a_configured_window_when_read_then_that_value_is_reported() {
    let handler = GetSettingsHandler::new(FakeAuth::Allowing, &settings_with(7));

    let view = handler.get(TOKEN).await.unwrap();

    assert_eq!(view.deletion.retention_days, 7);
}

/// A window of zero is a legitimate configuration — every deleted record is
/// purgeable at once — and must be reported rather than treated as unset.
#[tokio::test]
async fn given_a_zero_window_when_read_then_zero_is_reported() {
    let handler = GetSettingsHandler::new(FakeAuth::Allowing, &settings_with(0));

    let view = handler.get(TOKEN).await.unwrap();

    assert_eq!(view.deletion.retention_days, 0);
}

/// AF-01.
#[tokio::test]
async fn given_unauthenticated_when_read_then_unauthorized() {
    let handler = GetSettingsHandler::new(FakeAuth::Denying, &Settings::default());

    let err = handler.get(TOKEN).await.unwrap_err();

    assert!(matches!(err, DomainError::Unauthorized));
}
