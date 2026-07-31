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
}
