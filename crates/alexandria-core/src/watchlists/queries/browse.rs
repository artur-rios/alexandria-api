use uuid::Uuid;

use crate::errors::DomainError;
use crate::watchlists::model::WatchlistWithProgress;
use crate::watchlists::repos::WatchlistRepository;

/// Browse watchlists and their items' watch progress (UC-21 / FR-WL-08).
///
/// Generic over the watchlist repository, so the decision logic is unit-tested
/// against a trait fake, then wired with the concrete Sqlite collaborators at
/// runtime (services.rs). Both the HTTP and FFI surfaces call this handler so
/// the two stay at parity (FR-FC-24 / NFR-09).
pub struct BrowseWatchlistsHandler<R> {
    repo: R,
}

impl<R> BrowseWatchlistsHandler<R>
where
    R: WatchlistRepository,
{
    pub fn new(repo: R) -> Self {
        Self { repo }
    }

    /// List watchlists with their items' progress. When `watchlist_uuid` is
    /// `Some`, only that watchlist is returned (AF-01: `NotFound` when it
    /// does not exist); when `None`, every watchlist is returned.
    pub async fn list(
        &self,
        watchlist_uuid: Option<Uuid>,
    ) -> Result<Vec<WatchlistWithProgress>, DomainError> {
        let watchlists = match watchlist_uuid {
            Some(uuid) => {
                // AF-01: the requested watchlist must exist.
                let watchlist = self
                    .repo
                    .find_by_uuid(uuid)
                    .await?
                    .ok_or(DomainError::NotFound)?;
                vec![watchlist]
            }
            None => self.repo.list_all().await?,
        };

        let mut result = Vec::with_capacity(watchlists.len());
        for watchlist in watchlists {
            let items = self.repo.list_progress(watchlist.uuid).await?;
            result.push(WatchlistWithProgress {
                uuid: watchlist.uuid,
                name: watchlist.name,
                items,
            });
        }
        Ok(result)
    }
}
