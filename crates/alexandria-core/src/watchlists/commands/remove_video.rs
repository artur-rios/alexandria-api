use uuid::Uuid;

use crate::auth::AuthService;
use crate::errors::DomainError;
use crate::watchlists::model::WatchlistItemResult;
use crate::watchlists::repos::WatchlistRepository;

/// UC-24 — Remove a video from a watchlist (FR-WL-06). Deletes the
/// WatchProgress linking the video to the watchlist; the VideoFile itself is
/// preserved.
///
/// Generic over the auth service and the watchlist repository, so the
/// decision logic is unit-tested against a trait fake, then wired with the
/// concrete Bearer/Sqlite collaborators at runtime (services.rs). No `Clock`
/// or `Filesystem` collaborator — removing progress touches no timestamps
/// and nothing on disk.
pub struct RemoveVideoFromWatchlistHandler<A, R> {
    auth: A,
    repo: R,
}

impl<A, R> RemoveVideoFromWatchlistHandler<A, R>
where
    A: AuthService,
    R: WatchlistRepository,
{
    pub fn new(auth: A, repo: R) -> Self {
        Self { auth, repo }
    }

    /// Remove `video_uuid` from the watchlist identified by
    /// `watchlist_uuid`.
    pub async fn remove(
        &self,
        watchlist_uuid: Uuid,
        video_uuid: Uuid,
        token: &str,
    ) -> Result<WatchlistItemResult, DomainError> {
        // AF-02: the caller must be authenticated. Evaluated before the
        // WatchProgress is looked up (FR-AU-07 / SRD §7).
        self.auth.authenticate(token).await?;

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
