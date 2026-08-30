//! Registering, moving, and removing a library.

use uuid::Uuid;

use crate::auth::AuthService;
use crate::errors::DomainError;
use crate::libraries::model::{Library, NewLibrary};
use crate::libraries::repos::LibraryRepository;

/// What is wrong with a library's name, or `None` when it can be stored.
///
/// Only the check that would otherwise become a stored blank. Everything
/// else about the name is the owner's business.
pub fn validate_library_name(name: &str) -> Option<&'static str> {
    name.trim().is_empty().then_some("name is blank")
}

/// Register a folder as a library (libraries design).
pub struct RegisterLibraryHandler<A, R> {
    auth: A,
    repo: R,
}

impl<A, R> RegisterLibraryHandler<A, R>
where
    A: AuthService,
    R: LibraryRepository,
{
    pub fn new(auth: A, repo: R) -> Self {
        Self { auth, repo }
    }

    /// Register `root_path` under `name`, and claim what is already indexed
    /// beneath it.
    ///
    /// Claiming here rather than only at the next index run: a folder is
    /// usually marked *after* it has been indexed, and a library that showed
    /// nothing until the owner re-walked their disk would read as broken.
    pub async fn register(
        &self,
        name: &str,
        root_path: &str,
        token: &str,
    ) -> Result<Library, DomainError> {
        self.auth.authenticate(token).await?;

        if let Some(reason) = validate_library_name(name) {
            return Err(DomainError::InvalidInput(reason.to_string()));
        }
        if root_path.trim().is_empty() {
            return Err(DomainError::InvalidInput("root path is blank".to_string()));
        }

        // Refused rather than allowed to overlap, and the existing one is
        // named: "that folder is already inside Photography" is something
        // the owner can act on, where a bare refusal is a puzzle.
        if let Some(existing) = self.repo.find_overlapping(root_path, None).await? {
            return Err(DomainError::Conflict(format!(
                "that folder overlaps the library \"{}\"",
                existing.name
            )));
        }

        let library = self
            .repo
            .insert(NewLibrary {
                uuid: Uuid::new_v4(),
                name: name.trim().to_string(),
                root_path: root_path.trim().to_string(),
            })
            .await?;

        self.repo.claim_files(library.uuid).await?;

        Ok(library)
    }
}

/// Point a library at the folder it moved to (design section 1).
pub struct MoveLibraryHandler<A, R> {
    auth: A,
    repo: R,
}

impl<A, R> MoveLibraryHandler<A, R>
where
    A: AuthService,
    R: LibraryRepository,
{
    pub fn new(auth: A, repo: R) -> Self {
        Self { auth, repo }
    }

    /// Correct the library's root to `new_root`, bringing its files with it.
    ///
    /// A moved folder is a correction, not a re-index: the files are the same
    /// files, and re-walking the new location would mint new records and
    /// leave the old ones missing — losing every uuid, every watchlist place,
    /// and every reading position that pointed at them.
    ///
    /// The folder is not checked for existing on disk. The core is told a
    /// root and walks it; whether a path is there is answered by the walk,
    /// and refusing here would also refuse a drive that is merely unplugged
    /// at the moment the owner corrects the record.
    pub async fn move_to(
        &self,
        uuid: Uuid,
        new_root: &str,
        token: &str,
    ) -> Result<Library, DomainError> {
        self.auth.authenticate(token).await?;

        if new_root.trim().is_empty() {
            return Err(DomainError::InvalidInput("root path is blank".to_string()));
        }

        self.repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        // Excluding itself, or a library would be refused for overlapping
        // where it already is — and so could never move at all.
        if let Some(existing) = self.repo.find_overlapping(new_root, Some(uuid)).await? {
            return Err(DomainError::Conflict(format!(
                "that folder overlaps the library \"{}\"",
                existing.name
            )));
        }

        let (library, _moved) = self.repo.move_root(uuid, new_root.trim()).await?;

        // Claimed afterwards as well as moved: the destination may already
        // hold files the owner indexed before correcting the record, and they
        // belong to this library now for the same reason registering claims
        // what is already there.
        self.repo.claim_files(uuid).await?;

        Ok(library)
    }
}

/// Stop treating a folder as a library.
pub struct RemoveLibraryHandler<A, R> {
    auth: A,
    repo: R,
}

impl<A, R> RemoveLibraryHandler<A, R>
where
    A: AuthService,
    R: LibraryRepository,
{
    pub fn new(auth: A, repo: R) -> Self {
        Self { auth, repo }
    }

    /// Remove the library `uuid` identifies.
    ///
    /// The files are kept and return to the type panels. Marking a folder as
    /// a library empties part of a panel, which is not visible until after
    /// it is done — so the way back has to restore rather than delete, or an
    /// accidental marking costs the owner their catalog.
    pub async fn remove(&self, uuid: Uuid, token: &str) -> Result<(), DomainError> {
        self.auth.authenticate(token).await?;

        self.repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        self.repo.delete(uuid).await
    }
}
