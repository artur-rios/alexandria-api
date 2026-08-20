use crate::auth::AuthService;
use crate::collections::model::{CollectionKind, CollectionSummary};
use crate::collections::repos::CollectionRepository;
use crate::errors::DomainError;

/// UC-46 — Browse collections (FR-CO-08).
///
/// The read that answers "what collections do I have". Everything else in F-05
/// addresses a collection the caller already knows the uuid of; without this,
/// a client has no way to learn those uuids at all.
///
/// Generic over the collection repository alone: the item counts come from the
/// same query as the rows, so neither the catalog nor the bookmark repository
/// is involved.
pub struct ListCollectionsHandler<A, CR> {
    auth: A,
    collection_repo: CR,
}

impl<A, CR> ListCollectionsHandler<A, CR>
where
    A: AuthService,
    CR: CollectionRepository,
{
    pub fn new(auth: A, collection_repo: CR) -> Self {
        Self {
            auth,
            collection_repo,
        }
    }

    /// List the collections, optionally narrowed to `kind`.
    ///
    /// `None` is every collection. AF-01 — nothing to return — is an empty
    /// `Vec`, because an owner with no collections is in a state, not in an
    /// error.
    ///
    /// AF-02 does not appear here: an unrecognised `kind` cannot reach this
    /// handler, because the parameter is the domain enum. Both transports
    /// reject the unknown value while parsing their own request, which is what
    /// keeps the two answers identical (FR-FC-24 / NFR-09).
    pub async fn list(
        &self,
        kind: Option<CollectionKind>,
        token: &str,
    ) -> Result<Vec<CollectionSummary>, DomainError> {
        // AF-03: the caller must be authenticated. Evaluated before anything
        // is read (FR-AU-07 / SRD §7).
        self.auth.authenticate(token).await?;

        self.collection_repo.list_collections(kind).await
    }
}
