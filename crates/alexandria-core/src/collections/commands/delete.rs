use uuid::Uuid;

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
pub struct DeleteCollectionHandler<R> {
    repo: R,
}

impl<R> DeleteCollectionHandler<R>
where
    R: CollectionRepository,
{
    pub fn new(repo: R) -> Self {
        Self { repo }
    }

    /// Delete the collection identified by `uuid`, unlinking its items, and
    /// return the pre-delete record as confirmation.
    pub async fn delete(&self, uuid: Uuid) -> Result<Collection, DomainError> {
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
