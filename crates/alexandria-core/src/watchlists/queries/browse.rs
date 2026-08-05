use uuid::Uuid;

use crate::auth::AuthService;
use crate::errors::DomainError;
use crate::watchlists::model::WatchlistWithProgress;
use crate::watchlists::repos::WatchlistRepository;

/// Browse watchlists and their items' watch progress (UC-21 / FR-WL-08).
///
/// Generic over the auth service and the watchlist repository, so the
/// decision logic is unit-tested against a trait fake, then wired with the
/// concrete Bearer/Sqlite collaborators at runtime (services.rs). Both the
/// HTTP and FFI surfaces call this handler so the two stay at parity
/// (FR-FC-24 / NFR-09).
pub struct BrowseWatchlistsHandler<A, R> {
    auth: A,
    repo: R,
}

impl<A, R> BrowseWatchlistsHandler<A, R>
where
    A: AuthService,
    R: WatchlistRepository,
{
    pub fn new(auth: A, repo: R) -> Self {
        Self { auth, repo }
    }

    /// List watchlists with their items' progress. When `watchlist_uuid` is
    /// `Some`, only that watchlist is returned (AF-01: `NotFound` when it
    /// does not exist); when `None`, every watchlist is returned.
    pub async fn list(
        &self,
        watchlist_uuid: Option<Uuid>,
        token: &str,
    ) -> Result<Vec<WatchlistWithProgress>, DomainError> {
        // AF-02: the caller must be authenticated.
        self.auth.authenticate(token).await?;

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
