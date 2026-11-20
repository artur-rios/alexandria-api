//! Registering, moving, and removing a library.

use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::commands::index::{IndexStarted, LibraryRunStarter};
use crate::catalog::library_root::LibraryRoot;
use crate::catalog::runs::{CatalogRunRepository, RunPriority, RunStatus};
use crate::errors::DomainError;
use crate::libraries::model::{Library, NewLibrary, RegisteredLibrary};
use crate::libraries::repos::LibraryRepository;

/// What is wrong with a library's name, or `None` when it can be stored.
///
/// Only the check that would otherwise become a stored blank. Everything
/// else about the name is the owner's business.
pub fn validate_library_name(name: &str) -> Option<&'static str> {
    name.trim().is_empty().then_some("name is blank")
}

/// A root as the caller sent it, trimmed of surrounding whitespace and of
/// trailing separators, which is the one spelling that is checked, stored and
/// walked.
///
/// `/media/course/` and `/media/course` are one folder; storing the first
/// would make every prefix test against it look for `/media/course//`. A root
/// that is nothing but separators (`/`), or a drive (`D:\`), keeps its last
/// one, or it would stop naming a folder at all.
pub fn normalize_root(raw: &str) -> &str {
    let trimmed = raw.trim();
    let stripped = trimmed.trim_end_matches(['/', '\\']);
    if stripped.is_empty() || stripped.ends_with(':') {
        trimmed
    } else {
        stripped
    }
}

/// The refusal for a library whose run is not finished (FR-FC-43).
///
/// Names the run, because "cancel it first" is only something the owner can
/// act on when they know which run to cancel.
pub(crate) fn unfinished_run_conflict(run_id: Uuid) -> DomainError {
    DomainError::Conflict(format!(
        "the library has an unfinished run {run_id}; cancel it first"
    ))
}

/// Register a folder as a library and start indexing it (FR-FC-36).
pub struct RegisterLibraryHandler<A, R, S> {
    auth: A,
    repo: R,
    starter: S,
    /// `filesystem.root` (FR-FC-26): a library's folder is now the only root
    /// ever walked, so it is bounded here, before anything is stored.
    library_root: LibraryRoot,
}

impl<A, R, S> RegisterLibraryHandler<A, R, S>
where
    A: AuthService,
    R: LibraryRepository,
    S: LibraryRunStarter,
{
    pub fn new(auth: A, repo: R, starter: S, library_root: LibraryRoot) -> Self {
        Self {
            auth,
            repo,
            starter,
            library_root,
        }
    }

    /// Register `root_path` under `name` and open its first index run.
    ///
    /// Checked in this order, and a refusal stores nothing: the name, the
    /// root, `filesystem.root`, overlap with another library — and then the
    /// run's own checks (the folder exists). The library row has to exist
    /// before the run can name it, so a run that cannot open removes the row
    /// again rather than leaving a library nobody can index.
    ///
    /// The caller spawns the walk with the returned run id, as for every run.
    pub async fn register(
        &self,
        name: &str,
        root_path: &str,
        priority: RunPriority,
        token: &str,
    ) -> Result<RegisteredLibrary, DomainError> {
        self.auth.authenticate(token).await?;

        if let Some(reason) = validate_library_name(name) {
            return Err(DomainError::InvalidInput(reason.to_string()));
        }
        // Normalized once, here, and every use below is of the normalized
        // value. The overlap check used to run on what the caller sent while
        // the insert stored a trimmed form, so `"  /media/photos/2024"` was
        // compared as a path with two leading spaces — a prefix of nothing —
        // and then stored as a folder genuinely inside `/media/photos`.
        let root_path = normalize_root(root_path);
        if root_path.is_empty() {
            return Err(DomainError::InvalidInput("root path is blank".to_string()));
        }

        self.library_root.check_root(root_path)?;
        // Before overlap, so the owner hears what is wrong with the folder
        // they typed rather than about the library beside it; and before the
        // insert, so a folder no run could walk stores nothing at all.
        self.starter.check_folder(root_path).await?;

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
                root_path: root_path.to_string(),
            })
            .await?;

        match self
            .starter
            .start_library_run(&library, priority, token)
            .await
        {
            Ok(started) => Ok(RegisteredLibrary {
                library,
                run_id: started.run_id,
            }),
            Err(err) => {
                // Best effort: the run's own refusal is the error that
                // matters, and a failure to undo the row must not replace it.
                if let Err(undo) = self.repo.delete(library.uuid).await.map(|_| ()) {
                    tracing::warn!(
                        library = %library.uuid,
                        error = %undo,
                        "could not remove a library whose first run did not open"
                    );
                }
                Err(err)
            }
        }
    }
}

/// Re-index one library (UC-01).
pub struct IndexLibraryHandler<A, R, S, RR> {
    auth: A,
    repo: R,
    starter: S,
    runs: RR,
}

impl<A, R, S, RR> IndexLibraryHandler<A, R, S, RR>
where
    A: AuthService,
    R: LibraryRepository,
    S: LibraryRunStarter,
    RR: CatalogRunRepository,
{
    pub fn new(auth: A, repo: R, starter: S, runs: RR) -> Self {
        Self {
            auth,
            repo,
            starter,
            runs,
        }
    }

    /// Open an index run of the library `uuid` names, and answer the library
    /// with it — the caller walks `library.root_path`.
    ///
    /// Refused while the library already has an unfinished run (FR-FC-43):
    /// two walks of one folder would race each other's inserts, and a paused
    /// run is the owner's to resume or cancel rather than to be overtaken.
    pub async fn index(
        &self,
        uuid: Uuid,
        priority: RunPriority,
        token: &str,
    ) -> Result<(Library, IndexStarted), DomainError> {
        self.auth.authenticate(token).await?;

        let library = self
            .repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        if let Some(run) = self.runs.unfinished_for_library(uuid).await? {
            return Err(unfinished_run_conflict(run.id));
        }

        let started = self
            .starter
            .start_library_run(&library, priority, token)
            .await?;
        Ok((library, started))
    }
}

/// Point a library at the folder it moved to (design section 1).
pub struct MoveLibraryHandler<A, R, RR> {
    auth: A,
    repo: R,
    /// A resume walks the root frozen on the run's row, so moving under an
    /// unfinished run would send it walking the old folder (FR-FC-43).
    runs: RR,
    /// `filesystem.root` (FR-FC-26). A move rewrites every stored path under
    /// the library without walking the disk, and playback serves whatever
    /// path the catalog holds — so the new root is bounded exactly as an
    /// index root is, or a move would be the way around the index's bound.
    library_root: LibraryRoot,
}

impl<A, R, RR> MoveLibraryHandler<A, R, RR>
where
    A: AuthService,
    R: LibraryRepository,
    RR: CatalogRunRepository,
{
    pub fn new(auth: A, repo: R, library_root: LibraryRoot, runs: RR) -> Self {
        Self {
            auth,
            repo,
            library_root,
            runs,
        }
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
    ///
    /// It *is* checked against `filesystem.root` when one is configured
    /// (FR-FC-26), with the index's own check (`LibraryRoot`). That check is
    /// built to answer for a folder that does not exist yet: it resolves the
    /// longest part of the path that does exist and normalises the rest, so
    /// an unplugged drive mounted inside the root is still accepted while a
    /// `..` or a symbolic link that leads out of it is not.
    pub async fn move_to(
        &self,
        uuid: Uuid,
        new_root: &str,
        token: &str,
    ) -> Result<Library, DomainError> {
        self.auth.authenticate(token).await?;

        // Trimmed before it is either checked or stored, for the reason
        // `register` above trims: checking one spelling and writing another
        // lets a folder past the overlap rule it was meant to fail.
        let new_root = normalize_root(new_root);
        if new_root.is_empty() {
            return Err(DomainError::InvalidInput("root path is blank".to_string()));
        }

        self.repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        if let Some(run) = self.runs.unfinished_for_library(uuid).await? {
            return Err(unfinished_run_conflict(run.id));
        }

        self.library_root.check_root(new_root)?;

        // Excluding itself, or a library would be refused for overlapping
        // where it already is — and so could never move at all.
        if let Some(existing) = self.repo.find_overlapping(new_root, Some(uuid)).await? {
            return Err(DomainError::Conflict(format!(
                "that folder overlaps the library \"{}\"",
                existing.name
            )));
        }

        let (library, _moved) = self.repo.move_root(uuid, new_root).await?;

        Ok(library)
    }
}

/// Remove a library and its catalog records (FR-FC-40).
pub struct RemoveLibraryHandler<A, R, RR> {
    auth: A,
    repo: R,
    runs: RR,
}

impl<A, R, RR> RemoveLibraryHandler<A, R, RR>
where
    A: AuthService,
    R: LibraryRepository,
    RR: CatalogRunRepository,
{
    pub fn new(auth: A, repo: R, runs: RR) -> Self {
        Self { auth, repo, runs }
    }

    /// Remove the library `uuid` identifies, with every file record in it.
    ///
    /// Every file belongs to a library, so a removed library's records have
    /// nowhere else to be; the files on disk are never touched, and adding
    /// the folder again re-indexes them.
    ///
    /// Refused while one of its runs is *running* (FR-FC-43), naming it: the
    /// walk would go on writing rows into a library that is gone, and there
    /// is no waiting for it — a cancel only raises a signal, in whichever
    /// process the walk is in. A paused run is not writing, and goes with the
    /// library.
    pub async fn remove(&self, uuid: Uuid, token: &str) -> Result<(), DomainError> {
        self.auth.authenticate(token).await?;

        self.repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        if let Some(run) = self.runs.unfinished_for_library(uuid).await? {
            if run.status == RunStatus::Running {
                return Err(unfinished_run_conflict(run.id));
            }
        }

        self.repo.delete(uuid).await.map(|_removed| ())
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_root;

    #[test]
    fn given_trailing_separators_and_padding_when_normalized_then_they_are_dropped() {
        assert_eq!(normalize_root("  /media/course/  "), "/media/course");
        assert_eq!(normalize_root(r"D:\course\"), r"D:\course");
        assert_eq!(normalize_root("/media/course//"), "/media/course");
    }

    #[test]
    fn given_a_drive_or_filesystem_root_when_normalized_then_its_separator_stays() {
        assert_eq!(normalize_root(r"D:\"), r"D:\");
        assert_eq!(normalize_root("D:/"), "D:/");
        assert_eq!(normalize_root("/"), "/");
    }

    #[test]
    fn given_a_blank_root_when_normalized_then_it_is_empty() {
        assert_eq!(normalize_root("   "), "");
    }
}
