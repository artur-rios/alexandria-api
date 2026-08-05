use uuid::Uuid;

use crate::auth::AuthService;
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
pub struct DeleteReadingListHandler<A, R> {
    auth: A,
    repo: R,
}

impl<A, R> DeleteReadingListHandler<A, R>
where
    A: AuthService,
    R: ReadingListRepository,
{
    pub fn new(auth: A, repo: R) -> Self {
        Self { auth, repo }
    }

    /// Delete the reading list identified by `uuid`, removing its
    /// ReadingProgress entries, and return the pre-delete record as
    /// confirmation.
    pub async fn delete(&self, uuid: Uuid, token: &str) -> Result<ReadingList, DomainError> {
        // AF-02: the caller must be authenticated. Evaluated before the
        // reading list is looked up (FR-AU-07 / SRD §7), so an
        // unauthenticated caller learns nothing about whether the uuid
        // exists.
        self.auth.authenticate(token).await?;

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
