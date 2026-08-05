use thiserror::Error;

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
