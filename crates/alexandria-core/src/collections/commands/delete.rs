use uuid::Uuid;

use crate::auth::AuthService;
use crate::collections::model::Collection;
use crate::collections::repos::CollectionRepository;
use crate::errors::DomainError;

/// UC-12 — Delete a collection (FR-CO-04). Removes the collection while
/// preserving (unlinking) every item it contains — deleting a collection is
/// never a way to delete its contents.
///
/// Like the other collection handlers the command is the handler itself: no
/// `Clock` or `Filesystem` collaborator, since a collection carries no
/// timestamps and nothing on disk to compensate on failure.
pub struct DeleteCollectionHandler<A, R> {
    auth: A,
    repo: R,
}

impl<A, R> DeleteCollectionHandler<A, R>
where
    A: AuthService,
    R: CollectionRepository,
{
    pub fn new(auth: A, repo: R) -> Self {
        Self { auth, repo }
    }

    /// Delete the collection identified by `uuid`, unlinking its items, and
    /// return the pre-delete record as confirmation.
    pub async fn delete(&self, uuid: Uuid, token: &str) -> Result<Collection, DomainError> {
        // AF-02: the caller must be authenticated. Evaluated before the
        // collection is looked up (FR-AU-07 / SRD §7), so an unauthenticated
        // caller learns nothing about whether the uuid exists.
        self.auth.authenticate(token).await?;

        // AF-01: the collection must exist.
        let collection = self
            .repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        self.repo.delete_collection(uuid).await?;

        Ok(collection)
    }
}
