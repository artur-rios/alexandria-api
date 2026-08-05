use uuid::Uuid;

use crate::auth::AuthService;
use crate::errors::DomainError;
use crate::reading_lists::model::ReadingListItemResult;
use crate::reading_lists::repos::ReadingListRepository;

/// UC-30 — Remove an item from a reading list (FR-RL-06). Deletes the
/// ReadingProgress linking the item to the reading list; the file itself is
/// preserved. Mirrors `RemoveVideoFromWatchlistHandler` (UC-24).
///
/// Generic over the auth service and the reading list repository, so the
/// decision logic is unit-tested against a trait fake, then wired with the
/// concrete Bearer/Sqlite collaborators at runtime (services.rs). No
/// `Clock` or `Filesystem` collaborator — removing progress touches no
/// timestamps and nothing on disk.
pub struct RemoveItemFromReadingListHandler<A, R> {
    auth: A,
    repo: R,
}

impl<A, R> RemoveItemFromReadingListHandler<A, R>
where
    A: AuthService,
    R: ReadingListRepository,
{
    pub fn new(auth: A, repo: R) -> Self {
        Self { auth, repo }
    }

    /// Remove `item_uuid` from the reading list identified by
    /// `reading_list_uuid`.
    pub async fn remove(
        &self,
        reading_list_uuid: Uuid,
        item_uuid: Uuid,
        token: &str,
    ) -> Result<ReadingListItemResult, DomainError> {
        // AF-02: the caller must be authenticated. Evaluated before the
        // ReadingProgress is looked up (FR-AU-07 / SRD §7).
        self.auth.authenticate(token).await?;

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
