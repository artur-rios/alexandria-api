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
    /// external auth service is unreachable).
    #[error("service unavailable: {0}")]
    ServiceUnavailable(String),
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
}
