use uuid::Uuid;

use crate::errors::DomainError;
use crate::reading_lists::model::ReadingListItemResult;
use crate::reading_lists::repos::ReadingListRepository;

/// UC-30 — Remove an item from a reading list (FR-RL-06). Deletes the
/// ReadingProgress linking the item to the reading list; the file itself is
/// preserved. Mirrors `RemoveVideoFromWatchlistHandler` (UC-24).
///
/// Generic over the reading list repository, so the decision logic is
/// unit-tested against a trait fake, then wired with the concrete Sqlite
/// collaborators at runtime (services.rs). No `Clock` or `Filesystem`
/// collaborator — removing progress touches no timestamps and nothing on disk.
pub struct RemoveItemFromReadingListHandler<R> {
    repo: R,
}

impl<R> RemoveItemFromReadingListHandler<R>
where
    R: ReadingListRepository,
{
    pub fn new(repo: R) -> Self {
        Self { repo }
    }

    /// Remove `item_uuid` from the reading list identified by
    /// `reading_list_uuid`.
    pub async fn remove(
        &self,
        reading_list_uuid: Uuid,
        item_uuid: Uuid,
    ) -> Result<ReadingListItemResult, DomainError> {
        // AF-01: the ReadingProgress must exist. The repository's
        // `remove_progress` defends this in one statement (a zero-row
        // delete means it did not).
        self.repo
            .remove_progress(reading_list_uuid, item_uuid)
            .await?;

        Ok(ReadingListItemResult {
            reading_list_uuid,
            item_uuid,
        })
    }
}
