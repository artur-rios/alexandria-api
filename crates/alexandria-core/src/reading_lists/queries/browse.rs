use uuid::Uuid;

use crate::auth::AuthService;
use crate::errors::DomainError;
use crate::reading_lists::model::ReadingListWithProgress;
use crate::reading_lists::repos::ReadingListRepository;

/// Browse reading lists and their items' read progress (UC-27 / FR-RL-08).
///
/// Generic over the auth service and the reading list repository, so the
/// decision logic is unit-tested against a trait fake, then wired with the
/// concrete Bearer/Sqlite collaborators at runtime (services.rs). Both the
/// HTTP and FFI surfaces call this handler so the two stay at parity
/// (FR-FC-24 / NFR-09).
pub struct BrowseReadingListsHandler<A, R> {
    auth: A,
    repo: R,
}

impl<A, R> BrowseReadingListsHandler<A, R>
where
    A: AuthService,
    R: ReadingListRepository,
{
    pub fn new(auth: A, repo: R) -> Self {
        Self { auth, repo }
    }

    /// List reading lists with their items' progress. When
    /// `reading_list_uuid` is `Some`, only that reading list is returned
    /// (AF-01: `NotFound` when it does not exist); when `None`, every
    /// reading list is returned.
    pub async fn list(
        &self,
        reading_list_uuid: Option<Uuid>,
        token: &str,
    ) -> Result<Vec<ReadingListWithProgress>, DomainError> {
        // AF-02: the caller must be authenticated.
        self.auth.authenticate(token).await?;

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
