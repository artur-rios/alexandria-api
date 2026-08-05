use uuid::Uuid;

use crate::auth::AuthService;
use crate::errors::DomainError;
use crate::watchlists::model::Watchlist;
use crate::watchlists::repos::WatchlistRepository;

/// UC-25 — Delete a watchlist (FR-WL-07). Removes the watchlist and every
/// WatchProgress entry it holds; the VideoFiles themselves are preserved —
/// deleting a watchlist is never a way to delete the videos it tracked.
///
/// Like `DeleteCollectionHandler` the command is the handler itself: no
/// `Clock` or `Filesystem` collaborator, since a watchlist carries no
/// timestamps and nothing on disk to compensate on failure.
pub struct DeleteWatchlistHandler<A, R> {
    auth: A,
    repo: R,
}

impl<A, R> DeleteWatchlistHandler<A, R>
where
    A: AuthService,
    R: WatchlistRepository,
{
    pub fn new(auth: A, repo: R) -> Self {
        Self { auth, repo }
    }

    /// Delete the watchlist identified by `uuid`, removing its WatchProgress
    /// entries, and return the pre-delete record as confirmation.
    pub async fn delete(&self, uuid: Uuid, token: &str) -> Result<Watchlist, DomainError> {
        // AF-02: the caller must be authenticated. Evaluated before the
        // watchlist is looked up (FR-AU-07 / SRD §7), so an unauthenticated
        // caller learns nothing about whether the uuid exists.
        self.auth.authenticate(token).await?;

        // AF-01: the watchlist must exist.
        let watchlist = self
            .repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        self.repo.delete_watchlist(uuid).await?;

        Ok(watchlist)
    }
}
