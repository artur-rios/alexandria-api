use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::model::FileType;
use crate::catalog::repos::CatalogRepository;
use crate::errors::DomainError;
use crate::watchlists::model::WatchProgress;
use crate::watchlists::repos::WatchlistRepository;

/// UC-22 — Add a video to a watchlist (FR-WL-02, FR-WL-03). Links a
/// `VideoFile` to a watchlist, creating a `Pending` WatchProgress the first
/// time; adding an already-linked video is idempotent and returns the
/// existing progress unchanged rather than resetting it (no governing AF —
/// chosen to avoid clobbering UC-23 progress).
///
/// Generic over two repositories: `WatchlistRepository` to look up the
/// target watchlist and perform the link, `CatalogRepository` to look up the
/// video and confirm its type. No `Clock` or `Filesystem` collaborator.
pub struct AddVideoToWatchlistHandler<A, WR, CATR> {
    auth: A,
    watchlist_repo: WR,
    catalog_repo: CATR,
}

impl<A, WR, CATR> AddVideoToWatchlistHandler<A, WR, CATR>
where
    A: AuthService,
    WR: WatchlistRepository,
    CATR: CatalogRepository,
{
    pub fn new(auth: A, watchlist_repo: WR, catalog_repo: CATR) -> Self {
        Self {
            auth,
            watchlist_repo,
            catalog_repo,
        }
    }

    /// Link the video identified by `video_uuid` to the watchlist identified
    /// by `watchlist_uuid`.
    pub async fn add(
        &self,
        watchlist_uuid: Uuid,
        video_uuid: Uuid,
        token: &str,
    ) -> Result<WatchProgress, DomainError> {
        // AF-03: the caller must be authenticated. Evaluated before the
        // watchlist or video is looked up (FR-AU-07 / SRD §7).
        self.auth.authenticate(token).await?;

        // AF-02: the watchlist must exist.
        self.watchlist_repo
            .find_by_uuid(watchlist_uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        // AF-02: the video must exist.
        let file = self
            .catalog_repo
            .find_by_uuid(video_uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        // AF-01: the target file must be a VideoFile.
        if file.file_type != FileType::Video {
            return Err(DomainError::InvalidInput(format!(
                "file {video_uuid} is not a video"
            )));
        }

        self.watchlist_repo
            .add_video(watchlist_uuid, video_uuid)
            .await
    }
}
