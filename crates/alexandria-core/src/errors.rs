use std::collections::BTreeMap;
use std::fmt;

use serde::Serialize;
use thiserror::Error;

/// The `BEGIN` every explicit transaction in this workspace uses.
///
/// `BEGIN IMMEDIATE`, not the plain deferred `BEGIN` sqlx's `Pool::begin`
/// issues. Every transaction here writes, and most of them read first (resolve
/// a uuid to an internal id, then update that row). A deferred transaction
/// that reads before it writes has to upgrade its lock, and SQLite answers a
/// contended upgrade with `SQLITE_BUSY` **immediately** — the busy handler is
/// deliberately not invoked, because two transactions each waiting to upgrade
/// would deadlock. So the 5-second `busy_timeout` does not protect these; only
/// taking the write lock at `BEGIN` does.
///
/// This matters because UC-01/UC-02 now walk several files at a time
/// (`indexing.concurrency`), which puts the extraction writes
/// (`update_metadata`, `set_*`) in contention with each other. Those writes are
/// best-effort and only logged, so a `SQLITE_BUSY` there would silently lose
/// extracted metadata rather than fail loudly.
///
/// Applied to *every* explicit transaction rather than only the read-then-write
/// ones: all of them write, so none of them is harmed by taking the lock a few
/// statements early, and a single rule leaves no judgement call for the next
/// transaction someone adds.
pub const WRITE_TX: &str = "BEGIN IMMEDIATE";

#[derive(Debug, Error)]
pub enum DomainError {
    #[error("entity not found")]
    NotFound,
    #[error("unauthorized")]
    Unauthorized,
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// Input rejected for a reason the caller can act on programmatically
    /// (issue #101). `InvalidInput` plus a stable `code` and the parameters
    /// the message interpolates, so a client can render the rejection in its
    /// own language with the same bound the core enforced. Maps exactly where
    /// `InvalidInput` maps — `400` over HTTP, `AUTH_ERR_INVALID_INPUT` over
    /// FFI; the code is the reason, the status is only the class.
    #[error("invalid input: {0}")]
    Rejected(Rejection),
    #[error("invalid state transition")]
    InvalidState,
    /// A request that cannot be satisfied because it conflicts with state
    /// that already exists (UC-41 AF-01, AF-02). Distinct from
    /// `InvalidState`, which carries no message: registration has two
    /// different 409 conditions — wrong auth mode and account-already-
    /// exists — and a caller that cannot tell them apart cannot say
    /// anything useful to the owner.
    #[error("conflict: {0}")]
    Conflict(String),
    /// A filesystem operation failed (UC-05 AF-02, UC-09 AF-02, UC-32 AF-02,
    /// UC-33 AF-02). The catalog and the on-disk store stay consistent: the
    /// caller is told the disk operation failed and nothing was committed.
    #[error("disk error: {0}")]
    Disk(String),
    /// A write succeeded but the post-write content hash does not match the
    /// submitted bytes, even after one retry (UC-33 AF-03).
    #[error("integrity error: {0}")]
    Integrity(String),
    /// A required external dependency could not be reached (UC-36 AF-03: the
    /// external auth service is unreachable). Carries no reason code, and its
    /// message is deliberately not echoed — which of an installation's
    /// dependencies is down is not a caller's business.
    #[error("service unavailable: {0}")]
    ServiceUnavailable(String),
    /// A dependency is unavailable for a reason the caller *does* need — the
    /// mail transport is not configured, so the message it asked for will not
    /// arrive (issue #102). Stands to `ServiceUnavailable` exactly as
    /// `Rejected` stands to `InvalidInput`: same class, plus a stable code.
    #[error("service unavailable: {0}")]
    Unavailable(Rejection),
    /// A request refused because it came too soon after the last one
    /// (issue #102 / FR-AU-15: the confirmation resend interval). Its own
    /// variant rather than a `Conflict`, because it is not an error the caller
    /// made — it is a "not yet", and a client that can tell the two apart
    /// shows a countdown instead of an alarm. Carries the rejection so the
    /// wait can travel in `params`.
    #[error("too many requests: {0}")]
    TooManyRequests(Rejection),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("database migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error("configuration error: {0}")]
    Config(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl DomainError {
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::Conflict(message.into())
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    pub fn disk(message: impl Into<String>) -> Self {
        Self::Disk(message.into())
    }

    pub fn integrity(message: impl Into<String>) -> Self {
        Self::Integrity(message.into())
    }

    pub fn service_unavailable(message: impl Into<String>) -> Self {
        Self::ServiceUnavailable(message.into())
    }

    /// Reject input with a stable reason code (issue #101). `message` is the
    /// English fallback a log or a code-unaware client shows; `code` is what a
    /// client switches on. Add the values the message interpolates with
    /// [`Rejection::with_param`] so a client can rebuild the sentence itself:
    ///
    /// ```
    /// # use alexandria_core::errors::DomainError;
    /// DomainError::rejected("password_too_short", "password must be at least 12 characters")
    ///     .with_param("min", "12");
    /// ```
    pub fn rejected(code: &'static str, message: impl Into<String>) -> Self {
        Self::Rejected(Rejection::new(code, message))
    }

    /// A dependency is unavailable, named by a stable reason code the caller
    /// acts on (issue #102).
    pub fn unavailable(code: &'static str, message: impl Into<String>) -> Self {
        Self::Unavailable(Rejection::new(code, message))
    }

    /// Refuse a request that came too soon, naming the reason and the wait
    /// (issue #102 / FR-AU-15).
    pub fn too_many_requests(code: &'static str, message: impl Into<String>) -> Self {
        Self::TooManyRequests(Rejection::new(code, message))
    }

    /// Add a parameter to any rejection-carrying variant. A no-op on the
    /// others, so a `rejected(…).with_param(…)` chain reads straight through
    /// without unwrapping.
    #[must_use]
    pub fn with_param(self, key: impl Into<String>, value: impl Into<String>) -> Self {
        match self {
            Self::Rejected(rejection) => Self::Rejected(rejection.with_param(key, value)),
            Self::Unavailable(rejection) => Self::Unavailable(rejection.with_param(key, value)),
            Self::TooManyRequests(rejection) => {
                Self::TooManyRequests(rejection.with_param(key, value))
            }
            other => other,
        }
    }
}

/// An input rejection carrying the reason a client acts on (issue #101).
///
/// `code` is a stable `snake_case` identifier — never reused for a different
/// meaning — and `params` holds the values `message` interpolates. Parameters
/// are strings rather than free-form JSON: every one this surface has is a
/// bound or a name, and a flat string map is predictable for an FFI caller
/// parsing it by hand. The `BTreeMap` also sorts deterministically, so the
/// HTTP and FFI renderings can be compared byte-for-byte in a parity test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub code: &'static str,
    pub message: String,
    pub params: BTreeMap<String, String>,
}

impl Rejection {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            params: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn with_param(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.params.insert(key.into(), value.into());
        self
    }
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// The transport-independent class of a failure — what every surface agrees a
/// `DomainError` *is*, before either one names it in its own vocabulary. HTTP
/// maps this to a status code; FFI maps it to an `*_ERR_*` constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    NotFound,
    Unauthorized,
    BadRequest,
    Conflict,
    TooManyRequests,
    ServiceUnavailable,
    Internal,
}

/// The error envelope both surfaces emit (issue #101).
///
/// `error` is the human-readable English fallback and is always present —
/// unchanged from what HTTP has always returned. `code` and `params` appear
/// only for a [`DomainError::Rejected`]; an omitted `code` means this failure
/// has no stable identifier yet, which is honest and lets the rest of the
/// surface adopt codes incrementally.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ErrorBody {
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'static str>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, String>,
}

impl ErrorBody {
    fn plain(message: impl Into<String>) -> Self {
        Self {
            error: message.into(),
            code: None,
            params: BTreeMap::new(),
        }
    }

    /// Render as the JSON bytes both surfaces put on the wire.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| r#"{"error":"internal error"}"#.to_string())
    }
}

/// Render a `DomainError` as its class and its wire body.
///
/// The single place that decides what a failure looks like to a caller, so
/// parity between HTTP and FFI (FR-FC-24, FR-AU-08, NFR-09) is a property of
/// the code rather than of two `match` arms staying in step. Internal detail
/// (a disk path, a SQL error, a configuration value) is deliberately not
/// echoed: those variants render as their fixed class name and are logged, not
/// returned.
pub fn error_body(err: &DomainError) -> (ErrorClass, ErrorBody) {
    match err {
        DomainError::NotFound => (ErrorClass::NotFound, ErrorBody::plain("not found")),
        DomainError::Unauthorized => (ErrorClass::Unauthorized, ErrorBody::plain("unauthorized")),
        DomainError::InvalidInput(msg) => (ErrorClass::BadRequest, ErrorBody::plain(msg)),
        DomainError::Rejected(rejection) => (
            ErrorClass::BadRequest,
            ErrorBody {
                error: rejection.message.clone(),
                code: Some(rejection.code),
                params: rejection.params.clone(),
            },
        ),
        DomainError::InvalidState => (ErrorClass::Conflict, ErrorBody::plain("invalid state")),
        DomainError::Conflict(msg) => (ErrorClass::Conflict, ErrorBody::plain(msg)),
        DomainError::Disk(_) => (ErrorClass::Internal, ErrorBody::plain("disk error")),
        DomainError::Integrity(_) => (ErrorClass::Internal, ErrorBody::plain("integrity error")),
        DomainError::ServiceUnavailable(_) => (
            ErrorClass::ServiceUnavailable,
            ErrorBody::plain("service unavailable"),
        ),
        DomainError::Unavailable(rejection) => (
            ErrorClass::ServiceUnavailable,
            ErrorBody {
                error: rejection.message.clone(),
                code: Some(rejection.code),
                params: rejection.params.clone(),
            },
        ),
        DomainError::TooManyRequests(rejection) => (
            ErrorClass::TooManyRequests,
            ErrorBody {
                error: rejection.message.clone(),
                code: Some(rejection.code),
                params: rejection.params.clone(),
            },
        ),
        DomainError::Database(_) | DomainError::Migration(_) => {
            (ErrorClass::Internal, ErrorBody::plain("database error"))
        }
        DomainError::Config(_) => (
            ErrorClass::Internal,
            ErrorBody::plain("configuration error"),
        ),
        DomainError::Internal(_) => (ErrorClass::Internal, ErrorBody::plain("internal error")),
    }
}
