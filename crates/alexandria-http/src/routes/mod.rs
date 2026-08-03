pub mod browse;
pub mod edit_metadata;
pub mod health;
pub mod index;
pub mod refresh;
pub mod rename;
pub mod restore;
pub mod soft_delete;

use axum::http::HeaderMap;

/// Extract the bearer token from the `Authorization` header (case-insensitive
/// `Bearer ` prefix). Returns an empty string when the header is missing or
/// malformed — the handler then rejects it as `Unauthorized` (AF-02 / AF-03).
pub(crate) fn bearer_token(headers: &HeaderMap) -> String {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer ").or_else(|| s.strip_prefix("bearer ")))
        .unwrap_or("")
        .to_string()
}
