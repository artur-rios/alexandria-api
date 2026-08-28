use uuid::Uuid;

use crate::auth::AuthService;
use crate::errors::DomainError;
use crate::playlists::model::{Playlist, PlaylistView};
use crate::playlists::repos::PlaylistRepository;

/// Browse playlists and read one back with its tracks (Task 6).
///
/// Generic over the auth service and the playlist repository, so the
/// decision logic is unit-tested against a trait fake, then wired with the
/// concrete Bearer/Sqlite collaborators at runtime (services.rs). Both the
/// HTTP and FFI surfaces call this handler so the two stay at parity
/// (FR-FC-24 / NFR-09).
pub struct BrowsePlaylistsHandler<A, R> {
    auth: A,
    repo: R,
}

impl<A, R> BrowsePlaylistsHandler<A, R>
where
    A: AuthService,
    R: PlaylistRepository,
{
    pub fn new(auth: A, repo: R) -> Self {
        Self { auth, repo }
    }

    /// Every persisted playlist (without their tracks -- `read` answers a
    /// single playlist's tracks; a listing of every playlist's tracks would
    /// be an unbounded read no caller has asked for).
    pub async fn list(&self, token: &str) -> Result<Vec<Playlist>, DomainError> {
        // AF-02 equivalent: the caller must be authenticated.
        self.auth.authenticate(token).await?;
        self.repo.list_all().await
    }

    /// Read the playlist identified by `uuid` back with its tracks, in
    /// position order (design section 5 / the ordering the reorder use case
    /// exists to control). `NotFound` when the playlist does not exist.
    ///
    /// Each track is resolved via `PlaylistRepository::list_view`, which
    /// batches the file lookups rather than resolving one track at a time
    /// -- see that method's doc comment. A missing file's entry is kept and
    /// flagged (`missing: true`) rather than dropped.
    pub async fn read(&self, uuid: Uuid, token: &str) -> Result<PlaylistView, DomainError> {
        // Auth is checked before the payload is consulted (FR-AU-07 /
        // SRD §7).
        self.auth.authenticate(token).await?;

        let playlist = self
            .repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        let entries = self.repo.list_view(uuid).await?;

        Ok(PlaylistView { playlist, entries })
    }
}
