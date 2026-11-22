use uuid::Uuid;

use crate::errors::DomainError;
use crate::reading_lists::model::ReadingList;
use crate::reading_lists::repos::ReadingListRepository;

/// UC-31 — Delete a reading list (FR-RL-07). Removes the reading list and
/// every ReadingProgress entry it holds; the files themselves are
/// preserved — deleting a reading list is never a way to delete the items
/// it tracked. Mirrors `DeleteWatchlistHandler` (UC-25).
///
/// Like `DeleteWatchlistHandler` the command is the handler itself: no
/// `Clock` or `Filesystem` collaborator, since a reading list carries no
/// timestamps and nothing on disk to compensate on failure.
pub struct DeleteReadingListHandler<R> {
    repo: R,
}

impl<R> DeleteReadingListHandler<R>
where
    R: ReadingListRepository,
{
    pub fn new(repo: R) -> Self {
        Self { repo }
    }

    /// Delete the reading list identified by `uuid`, removing its
    /// ReadingProgress entries, and return the pre-delete record as
    /// confirmation.
    pub async fn delete(&self, uuid: Uuid) -> Result<ReadingList, DomainError> {
        // AF-01: the reading list must exist.
        let reading_list = self
            .repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        self.repo.delete_reading_list(uuid).await?;

        Ok(reading_list)
    }
}
