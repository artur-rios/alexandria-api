use uuid::Uuid;

use crate::auth::AuthService;
use crate::errors::DomainError;
use crate::playlists::model::PlaylistEntry;
use crate::playlists::repos::PlaylistRepository;

/// Add tracks to a playlist. Appends `file_uuids`, in order, at consecutive
/// positions after whatever the playlist already holds.
///
/// The whole slice succeeds or none of it does: `PlaylistRepository::
/// add_entries` resolves and validates every uuid inside one transaction, so
/// "add this whole album" is one call rather than N calls whose failure
/// halfway would leave half an album added. A playlist may hold the same
/// track more than once -- adding an already-present track appends a second
/// entry, it does not return the existing one (`playlist_entries` carries
/// no `UNIQUE (playlist_id, file_id)`, unlike `reading_progress`).
///
/// Like `CreatePlaylistHandler` and `DeletePlaylistHandler` the command is
/// the handler itself: no `Clock` or `Filesystem` collaborator. Generic
/// over `PlaylistRepository` alone -- the audio-type check happens inside
/// `add_entries`, not via a separate `CatalogRepository` lookup, because it
/// has to run inside the same transaction that resolves the uuid and picks
/// the insert position.
pub struct AddEntriesHandler<A, R> {
    auth: A,
    repo: R,
}

impl<A, R> AddEntriesHandler<A, R>
where
    A: AuthService,
    R: PlaylistRepository,
{
    pub fn new(auth: A, repo: R) -> Self {
        Self { auth, repo }
    }

    /// Add `file_uuids` to the playlist identified by `playlist_uuid` and
    /// return the new entries.
    pub async fn add(
        &self,
        playlist_uuid: Uuid,
        file_uuids: &[Uuid],
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

        self.repo.add_entries(playlist_uuid, file_uuids).await
    }
}
