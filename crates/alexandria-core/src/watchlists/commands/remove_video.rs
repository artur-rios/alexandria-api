use uuid::Uuid;

use crate::errors::DomainError;
use crate::watchlists::model::WatchlistItemResult;
use crate::watchlists::repos::WatchlistRepository;

/// UC-24 — Remove a video from a watchlist (FR-WL-06). Deletes the
/// WatchProgress linking the video to the watchlist; the VideoFile itself is
/// preserved.
///
/// Generic over the watchlist repository, so the decision logic is unit-tested
/// against a trait fake, then wired with the concrete Sqlite collaborators at
/// runtime (services.rs). No `Clock` or `Filesystem` collaborator — removing
/// progress touches no timestamps and nothing on disk.
pub struct RemoveVideoFromWatchlistHandler<R> {
    repo: R,
}

impl<R> RemoveVideoFromWatchlistHandler<R>
where
    R: WatchlistRepository,
{
    pub fn new(repo: R) -> Self {
        Self { repo }
    }

    /// Remove `video_uuid` from the watchlist identified by
    /// `watchlist_uuid`.
    pub async fn remove(
        &self,
        watchlist_uuid: Uuid,
        video_uuid: Uuid,
    ) -> Result<WatchlistItemResult, DomainError> {
        // AF-01: the WatchProgress must exist. The repository's
        // `remove_progress` defends this in one statement (a zero-row delete
        // means it did not).
        self.repo
            .remove_progress(watchlist_uuid, video_uuid)
            .await?;

        Ok(WatchlistItemResult {
            watchlist_uuid,
            video_uuid,
        })
    }
}
