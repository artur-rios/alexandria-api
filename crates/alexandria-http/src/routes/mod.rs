pub mod auth;
pub mod bookmarks;
pub mod browse;
pub mod collections;
pub mod delete_file;
pub mod edit_metadata;
pub mod enrichment;
pub mod health;
pub mod index;
pub mod libraries;
pub mod playback;
pub mod playlists;
pub mod reading_lists;
pub mod refresh;
pub mod rename;
pub mod restore;
pub mod runs;
pub mod settings;
pub mod text_content;
pub mod watchlists;

use axum::http::HeaderMap;
use serde::Deserialize;

use alexandria_core::catalog::runs::RunPriority;

/// Extract the bearer token from the `Authorization` header (case-insensitive
/// `Bearer ` prefix). Returns an empty string when the header is missing or
/// malformed — the handler then rejects it as `Unauthorized` (AF-02 / AF-03).
pub(crate) fn bearer_token(headers: &HeaderMap) -> String {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|s| {
            s.strip_prefix("Bearer ")
                .or_else(|| s.strip_prefix("bearer "))
        })
        .unwrap_or("")
        .to_string()
}

/// Lenient `priority` field deserializer shared by both start bodies
/// (`POST /v1/index`, `POST /v1/index/refresh` — Task 12). `"low"` maps to
/// `RunPriority::Low`; any other string — an unrecognised word, or a value
/// that is not a string at all — maps to `RunPriority::Normal` rather than
/// failing the request. Combined with `#[serde(default)]` on the field (which
/// covers the key being absent entirely), this makes an unreadable priority
/// behave exactly like an absent one.
///
/// This mirrors `alexandria-ffi::parse_priority` byte for byte (FR-FC-24): a
/// client that cannot spell the value gets the safe default on both
/// surfaces, never a rejected call on one and a silent fallback on the other.
pub(crate) fn deserialize_priority<'de, D>(deserializer: D) -> Result<RunPriority, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // `serde_json::Value` rather than `Option<String>`: a non-string value
    // (a number, an object) must fall back to `Normal` the same as an
    // unrecognised string, not bubble up as a type-mismatch error — the FFI
    // side has no way to send anything but a string in the first place, and
    // this keeps the two surfaces answering the same question ("did the
    // caller ask for `low`?") the same lenient way.
    let raw = serde_json::Value::deserialize(deserializer)?;
    Ok(match raw.as_str() {
        Some("low") => RunPriority::Low,
        _ => RunPriority::Normal,
    })
}

/// Lenient `priority` field deserializer for
/// `POST /v1/index/runs/{runId}/resume` (Task 15), where the answer is
/// three-valued rather than two: `"low"` and `"normal"` are requests to
/// re-pace the run, and **anything else — the key absent, an explicit
/// `null`, an unrecognised word, a value that is not a string — is `None`,
/// meaning keep the width the run already has.**
///
/// That is the one place this deliberately differs from
/// [`deserialize_priority`]. Starting a run must produce *some* width, so an
/// unreadable priority there falls to `Normal`; a run being resumed already
/// has a width, so falling to `Normal` would silently speed every
/// low-priority run back up the moment a client written before this field
/// existed sent the bodiless resume it has always sent. Combined with
/// `#[serde(default)]` on the field and `resume_run`'s folding of every
/// `JsonRejection` into `ResumeBody::default()`, "absent" means the same
/// thing at every level: no body, no field, or an unreadable field.
///
/// Mirrors `alexandria-ffi::parse_resume_priority` byte for byte (FR-FC-24).
pub(crate) fn deserialize_optional_priority<'de, D>(
    deserializer: D,
) -> Result<Option<RunPriority>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // `serde_json::Value` for the reason `deserialize_priority` uses it: a
    // non-string value must fall back rather than bubble up as a type
    // mismatch, so both surfaces answer the same question the same lenient
    // way. `Value` also absorbs an explicit `null` — `Option<String>` would
    // have reached the same answer by a different route, and one rule is
    // easier to keep true than two.
    let raw = serde_json::Value::deserialize(deserializer)?;
    Ok(match raw.as_str() {
        Some("low") => Some(RunPriority::Low),
        Some("normal") => Some(RunPriority::Normal),
        _ => None,
    })
}
