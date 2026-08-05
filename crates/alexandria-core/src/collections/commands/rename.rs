use uuid::Uuid;

use crate::auth::AuthService;
use crate::collections::commands::create::validate_collection_name;
use crate::collections::model::Collection;
use crate::collections::repos::CollectionRepository;
use crate::errors::DomainError;

/// UC-11 — Rename a collection (FR-CO-03). Renames an existing collection,
/// leaving its `kind` and members untouched.
///
/// Like `CreateCollectionHandler`, the command is the handler itself: no
/// `Clock` or `Filesystem` collaborator, since a collection carries no
/// timestamps and nothing on disk to compensate on failure.
pub struct RenameCollectionHandler<A, R> {
    auth: A,
    repo: R,
}

impl<A, R> RenameCollectionHandler<A, R>
where
    A: AuthService,
    R: CollectionRepository,
{
    pub fn new(auth: A, repo: R) -> Self {
        Self { auth, repo }
    }

    /// Rename the collection identified by `uuid` to `name`, and return the
    /// updated record.
    pub async fn rename(
        &self,
        uuid: Uuid,
        name: &str,
        token: &str,
    ) -> Result<Collection, DomainError> {
        // AF-03: the caller must be authenticated. Evaluated before the
        // collection is looked up or the name is validated (FR-AU-07 / SRD
        // §7), so an unauthenticated caller learns nothing about either.
        self.auth.authenticate(token).await?;

        // AF-02: the collection must exist.
        self.repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        // AF-01: the new name must be valid — same rule UC-10 applies at
        // creation, so the two use cases can never disagree on what a valid
        // collection name is.
        let name = validate_collection_name(name)?;

        self.repo.rename_collection(uuid, name).await
    }
}
