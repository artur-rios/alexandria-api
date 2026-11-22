use uuid::Uuid;

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
pub struct DeleteWatchlistHandler<R> {
    repo: R,
}

impl<R> DeleteWatchlistHandler<R>
where
    R: WatchlistRepository,
{
    pub fn new(repo: R) -> Self {
        Self { repo }
    }

    /// Delete the watchlist identified by `uuid`, removing its WatchProgress
    /// entries, and return the pre-delete record as confirmation.
    pub async fn delete(&self, uuid: Uuid) -> Result<Watchlist, DomainError> {
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
