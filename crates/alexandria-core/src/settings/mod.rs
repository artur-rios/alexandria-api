//! The configuration a client needs to render the catalog correctly
//! (F-00 / FR-FC-30).
//!
//! Deliberately narrow. This is not "the config" — most of what the server is
//! configured with is the operator's business and no client's: bind addresses,
//! database paths, the filesystem root, auth secrets. What lands here is the
//! settings a client cannot behave correctly without, and today that is one:
//! the retention window.

use serde::Serialize;

use crate::auth::AuthService;
use crate::config::{MetadataUnavailable, Settings};
use crate::errors::DomainError;

/// The deletion settings a client needs (FR-FC-30).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeletionSettingsView {
    /// How many days a soft-deleted record stays restorable.
    ///
    /// The boundary this number describes is the one the core enforces:
    /// elapsed time *up to and including* `retention_days` leaves a record
    /// restorable and not yet purgeable (UC-07); strictly past it, the record
    /// is purgeable and no longer restorable (UC-08, UC-19).
    pub retention_days: u32,
}

/// Whether music enrichment can be used, and if not, why.
///
/// A client needs both halves. Told only that it is unavailable, an
/// interface can offer the owner nothing but a dead menu item; told the
/// reason, it can say "your administrator has not turned this on" or "it is
/// on but needs a contact address configured" — two very different things to
/// do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataSettingsView {
    /// Whether an enrichment run would be accepted.
    pub available: bool,
    /// Why not, or `None` when it is available.
    pub unavailable_reason: Option<MetadataUnavailable>,
}

/// What UC-47 answers (FR-FC-30).
///
/// An object with one field rather than a bare number, so the next
/// client-relevant setting is a field here rather than a second endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsView {
    pub deletion: DeletionSettingsView,
    /// Reported so a client can render the enrichment surfaces honestly
    /// rather than offering them and watching every call fail.
    pub metadata: MetadataSettingsView,
}

/// UC-47 — Report the retention window (FR-FC-30).
///
/// It reads the settings this process was started with, so it costs no
/// database round trip and cannot disagree with what the restore and purge
/// handlers enforce: they are handed the same value from the same place.
pub struct GetSettingsHandler<A> {
    auth: A,
    settings: SettingsView,
}

impl<A> GetSettingsHandler<A>
where
    A: AuthService,
{
    pub fn new(auth: A, settings: &Settings) -> Self {
        Self {
            auth,
            settings: SettingsView {
                deletion: DeletionSettingsView {
                    retention_days: settings.deletion.retention_days,
                },
                metadata: MetadataSettingsView {
                    available: settings.metadata.is_available(),
                    unavailable_reason: settings.metadata.unavailable_reason(),
                },
            },
        }
    }

    /// Report the settings.
    pub async fn get(&self, token: &str) -> Result<SettingsView, DomainError> {
        // AF-01: the caller must be authenticated. The window is not a secret,
        // but every other `/v1` read takes a token and an exception here would
        // be one more thing to remember.
        self.auth.authenticate(token).await?;

        Ok(self.settings)
    }
}
