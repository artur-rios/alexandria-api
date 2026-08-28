use uuid::Uuid;

use crate::auth::AuthService;
use crate::errors::DomainError;
use crate::playlists::commands::create::validate_playlist_name;
use crate::playlists::model::Playlist;
use crate::playlists::repos::PlaylistRepository;

/// Rename a playlist. Renames an existing playlist, leaving its entries and
/// their order untouched. Mirrors `RenameCollectionHandler`.
///
/// Like `CreatePlaylistHandler` the command is the handler itself: no
/// `Clock` collaborator (a playlist carries no timestamps) and no
/// `Filesystem` (catalog-only metadata with nothing on disk).
pub struct RenamePlaylistHandler<A, R> {
    auth: A,
    repo: R,
}

impl<A, R> RenamePlaylistHandler<A, R>
where
    A: AuthService,
    R: PlaylistRepository,
{
    pub fn new(auth: A, repo: R) -> Self {
        Self { auth, repo }
    }

    /// Rename the playlist identified by `uuid` to `name` and return the
    /// updated record.
    pub async fn rename(
        &self,
        uuid: Uuid,
        name: &str,
        token: &str,
    ) -> Result<Playlist, DomainError> {
        // The caller must be authenticated. Evaluated before the playlist is
        // looked up or the name is validated (FR-AU-07 / SRD §7), so an
        // unauthenticated caller learns nothing about either.
        self.auth.authenticate(token).await?;

        // The playlist must exist.
        self.repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        // The new name must be valid -- the same rule creation applies, so
        // the two can never disagree on what a valid playlist name is.
        let name = validate_playlist_name(name)?;

        self.repo.rename_playlist(uuid, name).await
    }
}
