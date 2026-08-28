use uuid::Uuid;

use crate::auth::AuthService;
use crate::errors::DomainError;
use crate::playlists::repos::PlaylistRepository;

/// Remove one entry from a playlist. Addressed by `entry_uuid`, not by file
/// uuid: a playlist may hold the same track more than once (see
/// `AddEntriesHandler`), so a file uuid does not identify a row -- only the
/// entry's own uuid does.
///
/// Like `DeletePlaylistHandler` the command is the handler itself: no
/// `Clock` or `Filesystem` collaborator, since removing an entry carries no
/// timestamp and nothing on disk to compensate on failure.
pub struct RemoveEntryHandler<A, R> {
    auth: A,
    repo: R,
}

impl<A, R> RemoveEntryHandler<A, R>
where
    A: AuthService,
    R: PlaylistRepository,
{
    pub fn new(auth: A, repo: R) -> Self {
        Self { auth, repo }
    }

    /// Remove the entry identified by `entry_uuid` from the playlist
    /// identified by `playlist_uuid`.
    pub async fn remove(
        &self,
        playlist_uuid: Uuid,
        entry_uuid: Uuid,
        token: &str,
    ) -> Result<(), DomainError> {
        // The caller must be authenticated. Evaluated before the playlist
        // or entry is consulted (FR-AU-07 / SRD §7).
        self.auth.authenticate(token).await?;

        // The playlist must exist.
        self.repo
            .find_by_uuid(playlist_uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        // `remove_entry` itself confirms the entry belongs to this
        // playlist -- entry uuids are global, so without that check one
        // playlist could delete another's row.
        self.repo.remove_entry(playlist_uuid, entry_uuid).await
    }
}
