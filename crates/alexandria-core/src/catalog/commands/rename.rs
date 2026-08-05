use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::fs::Filesystem;
use crate::catalog::model::{File, FileState};
use crate::catalog::repos::CatalogRepository;
use crate::errors::DomainError;

/// Validate a new file name as a host-OS file name (UC-05 / FR-FC-19, AF-01).
/// Conservative enough to be valid on both Windows and POSIX so the HTTP and
/// FFI surfaces — and the cross-platform embedder — stay at parity (NFR-09)
/// without platform-specific rules leaking through one surface but not the
/// other.
///
/// Rejects: empty; leading/trailing whitespace (the trimmed form would differ
/// from what the caller typed, silently coercing a typo to a real file name);
/// names containing a NUL, a path separator (`/` or `\`); `.` and `..`; any of
/// `<>:"|?*`; a trailing dot (Windows); names longer than 255 bytes.
pub fn validate_file_name(name: &str) -> Result<String, DomainError> {
    if name.is_empty() {
        return Err(DomainError::InvalidInput("file name is required".into()));
    }
    if name != name.trim() {
        return Err(DomainError::InvalidInput(
            "file name must not have leading or trailing whitespace".into(),
        ));
    }
    let bytes = name.as_bytes();
    if bytes.len() > 255 {
        return Err(DomainError::InvalidInput(
            "file name is longer than 255 bytes".into(),
        ));
    }
    if name == "." || name == ".." {
        return Err(DomainError::InvalidInput(
            "file name must not be `.` or `..`".into(),
        ));
    }
    for b in bytes {
        // A stray NUL would terminate the C string before the FFI boundary
        // sees the full name; separators would turn the name into a path.
        if *b == 0 || *b == b'/' || *b == b'\\' {
            return Err(DomainError::InvalidInput(
                "file name must not contain a path separator or NUL".into(),
            ));
        }
    }
    for ch in ['<', '>', ':', '"', '|', '?', '*'] {
        if name.contains(ch) {
            return Err(DomainError::InvalidInput(format!(
                "file name must not contain `{ch}`"
            )));
        }
    }
    if name.ends_with('.') {
        return Err(DomainError::InvalidInput(
            "file name must not end with `.`".into(),
        ));
    }
    Ok(name.to_string())
}

/// Compute `parent(old_path).join(new_name)`, the in-place rename target.
/// Falls back to `new_name` as a path when `old_path` has no parent (it was
/// a bare file name the indexer somehow stored rootless), preserving the
/// catalog's existing invariant rather than silently dropping the directory.
/// Compute `parent(old_path).join(new_name)` as a string, preserving the
/// separator the indexer already stored in `old_path`. Using `Path::parent` +
/// `join` here would normalize to the OS separator (backslashes on Windows),
/// which desyncs from the catalog's stored `path` string and the FFI's
/// parity check; doing the slice entirely in `str` keeps the separator
/// end-to-end identical over both transports (FR-FC-24 / NFR-09).
fn sibling_path(old_path: &str, new_name: &str) -> String {
    match old_path.rfind(['/', '\\']) {
        Some(i) => {
            let mut s = String::with_capacity(i + 1 + new_name.len());
            s.push_str(&old_path[..=i]);
            s.push_str(new_name);
            s
        }
        None => new_name.to_string(),
    }
}

/// Rename a file (UC-05 / FR-FC-19). Renames the on-disk file and updates the
/// catalog's `name` and `path` atomically.
///
/// The handler authenticates the caller (AF-04), looks the file up by its
/// public UUID (AF-03), rejects renames on a soft-deleted record (restore via
/// UC-07 first), validates the new name as a host-OS file name (AF-01), and
/// then performs the on-disk rename before touching the catalog (AF-02 — if
/// the disk rename fails, no catalog change is ever made). If the disk rename
/// succeeds but the catalog write fails, the handler compensates by moving
/// the on-disk file back to its old path, so the end state is catalog
/// unchanged and on-disk file untouched (the same end state AF-02 calls for).
///
/// Target-exists is folded into AF-02: the handler rejects a rename onto a
/// path another cataloged file owns, or one an on-disk entry already sits at,
/// with a `Disk` error — before any on-disk move is attempted.
///
/// Generic over auth, catalog repository, and filesystem so the same decision
/// logic is unit-tested against trait fakes (no real DB, fs, or auth in unit
/// tests), then wired with the concrete collaborators at runtime (services.rs).
/// Both the HTTP and FFI surfaces call this handler so the two stay at parity
/// (FR-FC-24 / NFR-09).
pub struct RenameFileHandler<A, R, F> {
    auth: A,
    repo: R,
    fs: F,
}

impl<A, R, F> RenameFileHandler<A, R, F>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
{
    pub fn new(auth: A, repo: R, fs: F) -> Self {
        Self { auth, repo, fs }
    }

    /// Rename `uuid` to `new_name`. Returns the updated `File` on success.
    pub async fn rename(
        &self,
        uuid: Uuid,
        new_name: String,
        token: &str,
    ) -> Result<File, DomainError> {
        // AF-04: the caller must be authenticated.
        self.auth.authenticate(token).await?;

        // AF-03: the file must exist.
        let file = self
            .repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        // Precondition: restore via UC-07 before renaming a deleted record.
        if file.state == FileState::Deleted {
            return Err(DomainError::InvalidState);
        }

        // AF-01: validate the name as a host-OS file name.
        let new_name = validate_file_name(&new_name)?;

        // Same-name no-op: `std::fs::rename` to the same target path fails on
        // Windows, so short-circuit rather than surface a spurious disk error
        // for a request that changes nothing.
        if new_name == file.name {
            return Ok(file);
        }

        let new_path = sibling_path(&file.path, &new_name);

        // AF-02 (target-exists, cataloged): another file already owns the
        // target path — refuse before moving anything on disk.
        if let Some(existing) = self.repo.find_by_path(&new_path).await? {
            if existing.uuid != file.uuid {
                return Err(DomainError::disk(format!(
                    "target path already cataloged for a different file: {new_path}"
                )));
            }
        }

        // AF-02 (target-exists, on-disk): an on-disk entry already sits at the
        // target. `std::fs::rename` would replace it on POSIX (data loss); on
        // Windows it fails. Either way the right move is to refuse first.
        if self.fs.path_exists(&new_path).await {
            return Err(DomainError::disk(format!(
                "target path already exists on disk: {new_path}"
            )));
        }

        // AF-02 (disk failure): perform the on-disk rename before the catalog
        // write. If it fails, no catalog change was ever made, so there is
        // nothing to roll back and the on-disk file is untouched.
        self.fs.rename(&file.path, &new_path).await?;

        // If the catalog write fails after the disk rename, compensate by
        // moving the on-disk file back to its original path — the AF-02 end
        // state is "catalog unchanged, on-disk file untouched", not "catalog
        // unchanged, on-disk file silently moved".
        match self.repo.rename_file(uuid, &new_name, &new_path).await {
            Ok(updated) => Ok(updated),
            Err(err) => {
                // Best-effort rollback: log and recover the original path on
                // disk. If the rollback itself fails, the catalog is already
                // consistent (the tx rolled back) and only the disk is now in
                // the renamed state — surface that as a secondary disk error.
                if let Err(rollback) = self.fs.rename(&new_path, &file.path).await {
                    tracing::error!(
                        original = %file.path,
                        intermediate = %new_path,
                        error = %rollback,
                        "rename rollback failed; catalog rolled back but on-disk file is now at the intermediate path"
                    );
                }
                Err(err)
            }
        }
    }
}
