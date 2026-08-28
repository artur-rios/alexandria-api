use uuid::Uuid;

use crate::auth::AuthService;
use crate::errors::DomainError;
use crate::playlists::model::PlaylistEntry;
use crate::playlists::repos::PlaylistRepository;

/// Move one playlist entry to a new index. Addressed by `entry_uuid`, not by
/// file uuid, for the same reason as `RemoveEntryHandler`: a playlist may
/// hold the same track more than once, so a file uuid does not identify a
/// row.
///
/// The contract is "put entry X at index N": `PlaylistRepository::
/// move_entry` computes the new order and renumbers every entry in one
/// transaction. This handler does not accept or compute positions itself --
/// doing so here would be a second implementation of the ordering rule
/// living outside the core, and the two would drift (design, Risks; BR-02).
/// Returning the full new order lets a caller replace what it is showing
/// with what the core actually did, rather than predicting it.
///
/// Like `RemoveEntryHandler` the command is the handler itself: no `Clock`
/// or `Filesystem` collaborator, since reordering carries no timestamp and
/// nothing on disk to compensate on failure.
pub struct ReorderPlaylistHandler<A, R> {
    auth: A,
    repo: R,
}

impl<A, R> ReorderPlaylistHandler<A, R>
where
    A: AuthService,
    R: PlaylistRepository,
{
    pub fn new(auth: A, repo: R) -> Self {
        Self { auth, repo }
    }

    /// Move the entry identified by `entry_uuid` to `to_index` within the
    /// playlist identified by `playlist_uuid`, and return the playlist's
    /// full new order.
    pub async fn move_entry(
        &self,
        playlist_uuid: Uuid,
        entry_uuid: Uuid,
        to_index: i64,
        token: &str,
    ) -> Result<Vec<PlaylistEntry>, DomainError> {
        // The caller must be authenticated. Evaluated before the playlist
        // or payload is consulted (FR-AU-07 / SRD §7).
        self.auth.authenticate(token).await?;

        // The playlist must exist.
        self.repo
            .find_by_uuid(playlist_uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        // `move_entry` itself confirms the entry belongs to this playlist
        // and validates `to_index` -- entry uuids are global, and index
        // bounds depend on the playlist's current entry count, both of
        // which only the repository, inside its transaction, can check
        // consistently.
        self.repo
            .move_entry(playlist_uuid, entry_uuid, to_index)
            .await
    }
}
