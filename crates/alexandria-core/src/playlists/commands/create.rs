use uuid::Uuid;

use crate::auth::AuthService;
use crate::errors::DomainError;
use crate::playlists::model::{NewPlaylist, Playlist};
use crate::playlists::repos::PlaylistRepository;

/// Validate a playlist name. Rejects the same shapes
/// `validate_reading_list_name` does, for the same reasons (NFR-09 parity):
/// a playlist name is refused for the same reasons a reading list name is,
/// and a second, subtly different rule set would be the divergence.
///
/// Rejects: empty; whitespace-only; leading/trailing whitespace; names
/// containing a NUL; names longer than 255 bytes.
pub fn validate_playlist_name(name: &str) -> Result<String, DomainError> {
    if name.is_empty() {
        return Err(DomainError::InvalidInput(
            "playlist name is required".into(),
        ));
    }
    if name.trim().is_empty() {
        return Err(DomainError::InvalidInput(
            "playlist name must not be blank".into(),
        ));
    }
    if name != name.trim() {
        return Err(DomainError::InvalidInput(
            "playlist name must not have leading or trailing whitespace".into(),
        ));
    }
    if name.len() > 255 {
        return Err(DomainError::InvalidInput(
            "playlist name is longer than 255 bytes".into(),
        ));
    }
    if name.as_bytes().contains(&0) {
        return Err(DomainError::InvalidInput(
            "playlist name must not contain NUL".into(),
        ));
    }
    Ok(name.to_string())
}

/// Create a playlist. Creates a named, empty playlist for holding audio
/// files and returns the record carrying its new public UUID.
///
/// Like `CreateReadingListHandler` the command is the handler itself: no
/// `Clock` collaborator (a playlist carries no timestamps) and no
/// `Filesystem` (catalog-only metadata with nothing on disk).
pub struct CreatePlaylistHandler<A, R> {
    auth: A,
    repo: R,
}

impl<A, R> CreatePlaylistHandler<A, R>
where
    A: AuthService,
    R: PlaylistRepository,
{
    pub fn new(auth: A, repo: R) -> Self {
        Self { auth, repo }
    }

    /// Create a playlist named `name` and return the persisted record.
    pub async fn create(&self, name: &str, token: &str) -> Result<Playlist, DomainError> {
        // The caller must be authenticated. Evaluated before the payload is
        // consulted (FR-AU-07 / SRD §7).
        self.auth.authenticate(token).await?;

        // The name must be valid.
        let name = validate_playlist_name(name)?;

        self.repo
            .insert_playlist(NewPlaylist {
                uuid: Uuid::new_v4(),
                name,
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_a_blank_name_when_validated_then_invalid_input() {
        assert!(matches!(
            validate_playlist_name("   "),
            Err(DomainError::InvalidInput(_))
        ));
    }

    #[test]
    fn given_an_untrimmed_name_when_validated_then_invalid_input() {
        assert!(matches!(
            validate_playlist_name(" Road trip "),
            Err(DomainError::InvalidInput(_))
        ));
    }

    #[test]
    fn given_a_name_over_255_bytes_when_validated_then_invalid_input() {
        let long = "a".repeat(256);
        assert!(matches!(
            validate_playlist_name(&long),
            Err(DomainError::InvalidInput(_))
        ));
    }

    #[test]
    fn given_a_name_with_nul_when_validated_then_invalid_input() {
        assert!(matches!(
            validate_playlist_name("road\0trip"),
            Err(DomainError::InvalidInput(_))
        ));
    }

    #[test]
    fn given_a_valid_name_when_validated_then_it_is_returned() {
        assert_eq!(validate_playlist_name("Road trip").unwrap(), "Road trip");
    }
}
