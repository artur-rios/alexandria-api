use uuid::Uuid;

use crate::auth::AuthService;
use crate::errors::DomainError;
use crate::playlists::model::Playlist;
use crate::playlists::repos::PlaylistRepository;

/// Delete a playlist. Removes the playlist and every `playlist_entries` row
/// it holds; the referenced audio files are preserved -- deleting a
/// playlist is never a way to delete the files it lists. Mirrors
/// `DeleteReadingListHandler` / `DeleteCollectionHandler`.
///
/// Like `CreatePlaylistHandler` the command is the handler itself: no
/// `Clock` or `Filesystem` collaborator, since a playlist carries no
/// timestamps and nothing on disk to compensate on failure.
pub struct DeletePlaylistHandler<A, R> {
    auth: A,
    repo: R,
}

impl<A, R> DeletePlaylistHandler<A, R>
where
    A: AuthService,
    R: PlaylistRepository,
{
    pub fn new(auth: A, repo: R) -> Self {
        Self { auth, repo }
    }

    /// Delete the playlist identified by `uuid`, removing its entries, and
    /// return the pre-delete record as confirmation.
    pub async fn delete(&self, uuid: Uuid, token: &str) -> Result<Playlist, DomainError> {
        // The caller must be authenticated. Evaluated before the playlist
        // is looked up (FR-AU-07 / SRD §7), so an unauthenticated caller
        // learns nothing about whether the uuid exists.
        self.auth.authenticate(token).await?;

        // The playlist must exist.
        let playlist = self
            .repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        self.repo.delete_playlist(uuid).await?;

        Ok(playlist)
    }
}
