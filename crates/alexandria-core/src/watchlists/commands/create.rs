use uuid::Uuid;

use crate::auth::AuthService;
use crate::errors::DomainError;
use crate::watchlists::model::{NewWatchlist, Watchlist};
use crate::watchlists::repos::WatchlistRepository;

/// Validate a watchlist name (UC-20 / FR-WL-01, AF-01). The specification
/// requires only "non-empty"; the extra rules below exist for the same
/// reasons `validate_collection_name` applies them (NFR-09 parity).
///
/// Rejects: empty; whitespace-only; leading/trailing whitespace; names
/// containing a NUL; names longer than 255 bytes.
pub fn validate_watchlist_name(name: &str) -> Result<String, DomainError> {
    if name.is_empty() {
        return Err(DomainError::InvalidInput(
            "watchlist name is required".into(),
        ));
    }
    if name.trim().is_empty() {
        return Err(DomainError::InvalidInput(
            "watchlist name must not be blank".into(),
        ));
    }
    if name != name.trim() {
        return Err(DomainError::InvalidInput(
            "watchlist name must not have leading or trailing whitespace".into(),
        ));
    }
    if name.len() > 255 {
        return Err(DomainError::InvalidInput(
            "watchlist name is longer than 255 bytes".into(),
        ));
    }
    if name.as_bytes().contains(&0) {
        return Err(DomainError::InvalidInput(
            "watchlist name must not contain NUL".into(),
        ));
    }
    Ok(name.to_string())
}

/// UC-20 — Create a watchlist (FR-WL-01). Creates a named watchlist for
/// tracking video consumption and returns the record carrying its new
/// public UUID.
///
/// Like `CreateCollectionHandler` the command is the handler itself: no
/// `Clock` collaborator (a watchlist carries no timestamps) and no
/// `Filesystem` (catalog-only metadata with nothing on disk).
pub struct CreateWatchlistHandler<A, R> {
    auth: A,
    repo: R,
}

impl<A, R> CreateWatchlistHandler<A, R>
where
    A: AuthService,
    R: WatchlistRepository,
{
    pub fn new(auth: A, repo: R) -> Self {
        Self { auth, repo }
    }

    /// Create a watchlist named `name` and return the persisted record.
    pub async fn create(&self, name: &str, token: &str) -> Result<Watchlist, DomainError> {
        // AF-02: the caller must be authenticated. Evaluated before the
        // payload is consulted (FR-AU-07 / SRD §7).
        self.auth.authenticate(token).await?;

        // AF-01: the name must be valid.
        let name = validate_watchlist_name(name)?;

        self.repo
            .insert_watchlist(NewWatchlist {
                uuid: Uuid::new_v4(),
                name,
            })
            .await
    }
}
