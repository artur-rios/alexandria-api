use uuid::Uuid;

use crate::errors::DomainError;
use crate::reading_lists::model::ReadingListWithProgress;
use crate::reading_lists::repos::ReadingListRepository;

/// Browse reading lists and their items' read progress (UC-27 / FR-RL-08).
///
/// Generic over the reading list repository, so the decision logic is
/// unit-tested against a trait fake, then wired with the concrete Sqlite
/// collaborators at runtime (services.rs).
pub struct BrowseReadingListsHandler<R> {
    repo: R,
}

impl<R> BrowseReadingListsHandler<R>
where
    R: ReadingListRepository,
{
    pub fn new(repo: R) -> Self {
        Self { repo }
    }

    /// List reading lists with their items' progress. When
    /// `reading_list_uuid` is `Some`, only that reading list is returned
    /// (AF-01: `NotFound` when it does not exist); when `None`, every
    /// reading list is returned.
    pub async fn list(
        &self,
        reading_list_uuid: Option<Uuid>,
    ) -> Result<Vec<ReadingListWithProgress>, DomainError> {
        let reading_lists = match reading_list_uuid {
            Some(uuid) => {
                // AF-01: the requested reading list must exist.
                let reading_list = self
                    .repo
                    .find_by_uuid(uuid)
                    .await?
                    .ok_or(DomainError::NotFound)?;
                vec![reading_list]
            }
            None => self.repo.list_all().await?,
        };

        let mut result = Vec::with_capacity(reading_lists.len());
        for reading_list in reading_lists {
            let items = self.repo.list_progress(reading_list.uuid).await?;
            result.push(ReadingListWithProgress {
                uuid: reading_list.uuid,
                name: reading_list.name,
                items,
            });
        }
        Ok(result)
    }
}
