use uuid::Uuid;

use crate::auth::AuthService;
use crate::errors::DomainError;
use crate::watchlists::model::{WatchProgress, WatchState};
use crate::watchlists::repos::WatchlistRepository;

/// Whether advancing a WatchProgress from `from` to `to` is a valid
/// transition (UC-23 / FR-WL-04). The WatchProgress lifecycle (Use Case
/// Specification Document §4.2) only defines two forward edges: `Pending` →
/// `Watching` and `Watching` → `Watched`. Anything else — going backward,
/// skipping a state, or resubmitting the current state — is rejected
/// (AF-01), so the state machine can only ever move forward one step.
pub fn is_valid_transition(from: WatchState, to: WatchState) -> bool {
    matches!(
        (from, to),
        (WatchState::Pending, WatchState::Watching) | (WatchState::Watching, WatchState::Watched)
    )
}

/// UC-23 — Update watch progress (FR-WL-04, FR-WL-05). Advances a video's
/// watch state on a watchlist, recording the current episode for a series.
///
/// Generic over the auth service and the watchlist repository, so the
/// decision logic is unit-tested against a trait fake, then wired with the
/// concrete Bearer/Sqlite collaborators at runtime (services.rs). No `Clock`
/// or `Filesystem` collaborator — updating progress touches no timestamps
/// and nothing on disk.
pub struct UpdateWatchProgressHandler<A, R> {
    auth: A,
    repo: R,
}

impl<A, R> UpdateWatchProgressHandler<A, R>
where
    A: AuthService,
    R: WatchlistRepository,
{
    pub fn new(auth: A, repo: R) -> Self {
        Self { auth, repo }
    }

    /// Update the WatchProgress linking `video_uuid` to `watchlist_uuid` to
    /// `state`, replacing `current_episode`/`total_episodes` with the given
    /// values (full replace, not a merge — `None` clears the field).
    pub async fn update(
        &self,
        watchlist_uuid: Uuid,
        video_uuid: Uuid,
        state: WatchState,
        current_episode: Option<i64>,
        total_episodes: Option<i64>,
        token: &str,
    ) -> Result<WatchProgress, DomainError> {
        // AF-03: the caller must be authenticated. Evaluated before the
        // WatchProgress is looked up (FR-AU-07 / SRD §7).
        self.auth.authenticate(token).await?;

        // AF-02: a WatchProgress must exist for the video on that watchlist.
        let current = self
            .repo
            .find_progress(watchlist_uuid, video_uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        // AF-01: the requested transition must be valid.
        if !is_valid_transition(current.state, state) {
            return Err(DomainError::InvalidState);
        }

        self.repo
            .update_progress(
                watchlist_uuid,
                video_uuid,
                state,
                current_episode,
                total_episodes,
            )
            .await
    }
}
