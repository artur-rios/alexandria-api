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
}

/// Real on-disk filesystem backed by `walkdir` and `sha2`.
#[derive(Debug, Default, Clone, Copy)]
pub struct StdFilesystem;

impl StdFilesystem {
    fn collect(root: &Path) -> Vec<FileEntry> {
        let mut entries = Vec::new();
        for entry in walkdir::WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
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
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        Ok(format!("{:x}", hasher.finalize()))
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