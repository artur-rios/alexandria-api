use uuid::Uuid;

use crate::auth::AuthService;
use crate::errors::DomainError;
use crate::reading_lists::model::{NewReadingList, ReadingList};
use crate::reading_lists::repos::ReadingListRepository;

/// Validate a reading list name (UC-26 / FR-RL-01, AF-01). The specification
/// requires only "non-empty"; the extra rules below exist for the same
/// reasons `validate_watchlist_name` applies them (NFR-09 parity).
///
/// Rejects: empty; whitespace-only; leading/trailing whitespace; names
/// containing a NUL; names longer than 255 bytes.
pub fn validate_reading_list_name(name: &str) -> Result<String, DomainError> {
    if name.is_empty() {
        return Err(DomainError::InvalidInput(
            "reading list name is required".into(),
        ));
    }
    if name.trim().is_empty() {
        return Err(DomainError::InvalidInput(
            "reading list name must not be blank".into(),
        ));
    }
    if name != name.trim() {
        return Err(DomainError::InvalidInput(
            "reading list name must not have leading or trailing whitespace".into(),
        ));
    }
    if name.len() > 255 {
        return Err(DomainError::InvalidInput(
            "reading list name is longer than 255 bytes".into(),
        ));
    }
    if name.as_bytes().contains(&0) {
        return Err(DomainError::InvalidInput(
            "reading list name must not contain NUL".into(),
        ));
    }
    Ok(name.to_string())
}

/// UC-26 — Create a reading list (FR-RL-01). Creates a named reading list
/// for tracking book/comic consumption and returns the record carrying its
/// new public UUID.
///
/// Like `CreateWatchlistHandler` the command is the handler itself: no
/// `Clock` collaborator (a reading list carries no timestamps) and no
/// `Filesystem` (catalog-only metadata with nothing on disk).
pub struct CreateReadingListHandler<A, R> {
    auth: A,
    repo: R,
}

impl<A, R> CreateReadingListHandler<A, R>
where
    A: AuthService,
    R: ReadingListRepository,
{
    pub fn new(auth: A, repo: R) -> Self {
        Self { auth, repo }
    }

    /// Create a reading list named `name` and return the persisted record.
    pub async fn create(&self, name: &str, token: &str) -> Result<ReadingList, DomainError> {
        // AF-02: the caller must be authenticated. Evaluated before the
        // payload is consulted (FR-AU-07 / SRD §7).
        self.auth.authenticate(token).await?;

        // AF-01: the name must be valid.
        let name = validate_reading_list_name(name)?;

        self.repo
            .insert_reading_list(NewReadingList {
                uuid: Uuid::new_v4(),
                name,
            })
            .await
    }
}
