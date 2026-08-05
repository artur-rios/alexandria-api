use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::errors::DomainError;

/// A discovered file ready to be classified and hashed.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: String,
    pub name: String,
}

/// Filesystem port — the indexer's view of the on-disk store. The real
/// implementation walks the tree and streams bytes through SHA-256; unit
/// tests substitute an in-memory fake returning canned entries and hashes
/// (Testing Specification §6.2 — no real filesystem in unit tests).
#[allow(async_fn_in_trait)]
pub trait Filesystem: Send + Sync {
    async fn path_exists(&self, root: &str) -> bool;
    async fn list_files(&self, root: &str) -> Result<Vec<FileEntry>, DomainError>;
    async fn content_hash(&self, path: &str) -> Result<String, DomainError>;
    /// Rename `from` to `to` on disk (UC-05 / FR-FC-19). Atomic on a single
    /// volume; fails with `Disk` when the source is missing, the parent
    /// directory is not writable, or the target already exists (the latter is
    /// OS-dependent, so callers also pre-check it). Used by the rename
    /// handler, which leaves the catalog untouched if this fails (AF-02).
    async fn rename(&self, from: &str, to: &str) -> Result<(), DomainError>;
    /// Delete the file at `path` (UC-09 / FR-FC-23). `Ok(true)` — the file was
    /// present and is now gone. `Ok(false)` — no file was there (AF-01); the
    /// caller still removes the record and reports the absence. `Err(Disk)` —
    /// the delete failed (permission denied, AF-02); nothing was removed.
    async fn remove_file(&self, path: &str) -> Result<bool, DomainError>;
    /// Read the file at `path` as UTF-8 text (UC-32 / FR-TX-01). Fails with
    /// `Disk` when the file is missing, unreadable (permission), or its
    /// bytes are not valid UTF-8 (AF-02).
    async fn read_file(&self, path: &str) -> Result<String, DomainError>;
    /// Write `content` to `path`, replacing its bytes (UC-33 / FR-TX-02).
    /// Fails with `Disk` when the write cannot complete (disk full,
    /// permission denied — AF-02); the caller is responsible for leaving
    /// the catalog untouched when this fails.
    async fn write_file(&self, path: &str, content: &str) -> Result<(), DomainError>;
}

/// SHA-256 of `bytes`, lowercase hex (UC-01/UC-02/UC-33). Extracted so
/// `StdFilesystem::content_hash` and the UC-33 handler's pre-write hash
/// computation share one implementation and can never silently diverge —
/// UC-33 AF-03 relies on comparing this exact output against a post-write
/// `content_hash` read back from disk.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    // Lowercase hex, written byte by byte. digest 0.11 returns an `Array`
    // that no longer implements `LowerHex`, so `{:x}` is unavailable — but
    // the output must stay identical to what earlier versions produced,
    // because these hashes are persisted and compared on every re-index.
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Real on-disk filesystem backed by `walkdir` and `sha2`.
#[derive(Debug, Default, Clone, Copy)]
pub struct StdFilesystem;

impl StdFilesystem {
    fn collect(root: &Path) -> Vec<FileEntry> {
        let mut entries = Vec::new();
        for entry in walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path().to_path_buf();
            let name = entry
                .file_name()
                .to_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                continue;
            }
            entries.push(FileEntry {
                path: path.to_string_lossy().into_owned(),
                name,
            });
        }
        entries
    }
}

impl Filesystem for StdFilesystem {
    async fn path_exists(&self, root: &str) -> bool {
        Path::new(root).exists()
    }

    async fn list_files(&self, root: &str) -> Result<Vec<FileEntry>, DomainError> {
        let root = PathBuf::from(root);
        if !root.exists() {
            return Err(DomainError::InvalidInput("root path does not exist".into()));
        }
        Ok(Self::collect(&root))
    }

    async fn content_hash(&self, path: &str) -> Result<String, DomainError> {
        let bytes = std::fs::read(path)
            .map_err(|e| DomainError::Internal(format!("failed to read {}: {e}", path)))?;
        Ok(sha256_hex(&bytes))
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), DomainError> {
        std::fs::rename(from, to)
            .map_err(|e| DomainError::disk(format!("rename {from:?} -> {to:?}: {e}")))
    }

    async fn remove_file(&self, path: &str) -> Result<bool, DomainError> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(DomainError::disk(format!("remove {path:?}: {e}"))),
        }
    }

    async fn read_file(&self, path: &str) -> Result<String, DomainError> {
        std::fs::read_to_string(path).map_err(|e| DomainError::disk(format!("read {path:?}: {e}")))
    }

    async fn write_file(&self, path: &str, content: &str) -> Result<(), DomainError> {
        std::fs::write(path, content.as_bytes())
            .map_err(|e| DomainError::disk(format!("write {path:?}: {e}")))
    }
}

impl FileEntry {
    pub fn new(path: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            name: name.into(),
        }
    }
}
