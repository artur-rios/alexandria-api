use uuid::Uuid;

use crate::auth::AuthService;
use crate::collections::model::{Collection, CollectionKind, NewCollection};
use crate::collections::repos::CollectionRepository;
use crate::errors::DomainError;

/// Validate a collection name (UC-10 / FR-CO-01, FR-CO-02, AF-01). The
/// specification requires only "non-empty"; the extra rules below exist to keep
/// the two transports storing the same bytes for the same request (NFR-09).
///
/// Rejects: empty; whitespace-only; leading/trailing whitespace (trimming would
/// silently store a name different from the one the caller sent); names
/// containing a NUL (which would truncate the string at the FFI boundary, so
/// HTTP and FFI would disagree); names longer than 255 bytes.
///
/// Unlike a file name this is a label, not a path — separators and characters
/// such as `:` or `*` are legitimate in a collection name and are allowed.
pub fn validate_collection_name(name: &str) -> Result<String, DomainError> {
    if name.is_empty() {
        return Err(DomainError::InvalidInput(
            "collection name is required".into(),
        ));
    }
    if name.trim().is_empty() {
        return Err(DomainError::InvalidInput(
            "collection name must not be blank".into(),
        ));
    }
    if name != name.trim() {
        return Err(DomainError::InvalidInput(
            "collection name must not have leading or trailing whitespace".into(),
        ));
    }
    if name.len() > 255 {
        return Err(DomainError::InvalidInput(
            "collection name is longer than 255 bytes".into(),
        ));
    }
    if name.as_bytes().contains(&0) {
        return Err(DomainError::InvalidInput(
            "collection name must not contain NUL".into(),
        ));
    }
    Ok(name.to_string())
}

/// UC-10 — Create a collection (FR-CO-01, FR-CO-02). Creates a flat grouping
/// of files or bookmarks, fixed to one `kind` at creation, and returns the
/// record carrying its new public UUID.
///
/// Like the catalog's lifecycle handlers the command is the handler itself (no
/// separate `Command` struct): construction wires the collaborators
/// (`AuthService`, `CollectionRepository`) and `create` is the domain entry
/// point. There is no `Clock` collaborator — a collection carries no
/// timestamps (SRD §4.3) — and no `Filesystem`: a collection is catalog-only
/// metadata with nothing on disk to compensate on failure.
pub struct CreateCollectionHandler<A, R> {
    auth: A,
    repo: R,
}

impl<A, R> CreateCollectionHandler<A, R>
where
    A: AuthService,
    R: CollectionRepository,
{
    pub fn new(auth: A, repo: R) -> Self {
        Self { auth, repo }
    }

    /// Create a collection named `name` holding items of `kind`, and return
    /// the persisted record.
    pub async fn create(
        &self,
        name: &str,
        kind: CollectionKind,
        token: &str,
    ) -> Result<Collection, DomainError> {
        // AF-02: the caller must be authenticated. Evaluation happens before
        // the payload is consulted (FR-AU-07 / SRD §7), so an unauthenticated
        // caller never learns whether its name would have been accepted.
        self.auth.authenticate(token).await?;

        // AF-01: the name must be valid. An invalid `kind` cannot reach here —
        // it is a typed enum, so both transports reject an unrecognised value
        // while deserializing the request body.
        let name = validate_collection_name(name)?;

        // The uuid is minted here rather than in the repository so both
        // transports mint it the same way and a unit test can assert the
        // returned record against what the fake stored.
        self.repo
            .insert_collection(NewCollection {
                uuid: Uuid::new_v4(),
                name,
                kind,
            })
            .await
    }
}
