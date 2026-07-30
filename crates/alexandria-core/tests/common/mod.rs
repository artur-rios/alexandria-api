//! Shared test doubles for alexandria-core integration tests (Testing
//! Specification §6.2): hand-written fakes implementing the catalog
//! collaborators with no real database, filesystem, or auth service.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};

use alexandria_core::auth::{AuthService, Principal};
use alexandria_core::catalog::clock::FixedClock;
use alexandria_core::catalog::fs::{FileEntry, Filesystem};
use alexandria_core::catalog::model::{File, FileType, NewFile};
use alexandria_core::catalog::repos::CatalogRepository;
use alexandria_core::config::AuthMode;
use alexandria_core::errors::DomainError;

/// Fake auth service. `Denying` rejects every caller (AF-02); `Allowing`
/// authenticates any token as the owner.
#[derive(Debug, Clone, Copy)]
pub enum FakeAuth {
    Allowing,
    #[allow(dead_code)]
    Denying,
}

impl Default for FakeAuth {
    fn default() -> Self {
        FakeAuth::Allowing
    }
}

impl AuthService for FakeAuth {
    async fn authenticate(&self, _token: &str) -> Result<Principal, DomainError> {
        match self {
            FakeAuth::Allowing => Ok(Principal {
                user_id: "owner".to_string(),
            }),
            FakeAuth::Denying => Err(DomainError::Unauthorized),
        }
    }

    fn mode(&self) -> AuthMode {
        AuthMode::External
    }
}

/// In-memory catalog repository. `find_by_path` answers from an internal map;
/// `insert_file` records the file and surfaced the subtype decision via the
/// file's `file_type`. Backed by a shared `Arc<Mutex<…>>` so a test can clone
/// the repo, hand the original to the handler, and inspect the clone after the
/// run (the handler owns its `R`, but the shared map is the same one).
#[derive(Debug, Default, Clone)]
pub struct FakeCatalogRepository {
    files: Arc<Mutex<HashMap<String, File>>>,
}

impl FakeCatalogRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-seed an existing file at `path` so the indexer skips it (AF-03).
    pub fn with_existing(file: File) -> Self {
        let repo = Self::new();
        repo.files
            .lock()
            .unwrap()
            .insert(file.path.clone(), file);
        repo
    }

    pub fn count(&self) -> usize {
        self.files.lock().unwrap().len()
    }

    pub fn file_for(&self, path: &str) -> Option<File> {
        self.files.lock().unwrap().get(path).cloned()
    }

    pub fn has_path(&self, path: &str) -> bool {
        self.files.lock().unwrap().contains_key(path)
    }
}

impl CatalogRepository for FakeCatalogRepository {
    async fn find_by_path(&self, path: &str) -> Result<Option<File>, DomainError> {
        Ok(self.files.lock().unwrap().get(path).cloned())
    }

    async fn insert_file(&self, new_file: NewFile) -> Result<File, DomainError> {
        let file = File {
            uuid: new_file.uuid,
            path: new_file.path.clone(),
            name: new_file.name,
            file_type: new_file.file_type,
            content_hash: new_file.content_hash,
            state: alexandria_core::catalog::model::FileState::Active,
            deleted_at: None,
            indexed_at: new_file.indexed_at,
        };
        self.files.lock().unwrap().insert(new_file.path, file.clone());
        Ok(file)
    }
}

/// In-memory filesystem. Stores which roots "exist" and a map of root -> the
/// entries `list_files` returns, plus a map of path -> content hash.
#[derive(Debug, Default)]
pub struct FakeFilesystem {
    roots: std::collections::HashSet<String>,
    entries_by_root: HashMap<String, Vec<FileEntry>>,
    hash_by_path: HashMap<String, String>,
}

impl FakeFilesystem {
    pub fn builder() -> FakeFilesystemBuilder {
        FakeFilesystemBuilder::default()
    }
}

#[derive(Debug, Default)]
pub struct FakeFilesystemBuilder {
    fs: FakeFilesystem,
}

impl FakeFilesystemBuilder {
    pub fn with_root(mut self, root: &str) -> Self {
        self.fs.roots.insert(root.to_string());
        self.fs.entries_by_root.entry(root.to_string()).or_default();
        self
    }

    pub fn with_file(mut self, root: &str, path: &str, name: &str, hash: &str) -> Self {
        self.fs.roots.insert(root.to_string());
        self.fs
            .entries_by_root
            .entry(root.to_string())
            .or_default()
            .push(FileEntry::new(path, name));
        self.fs.hash_by_path.insert(path.to_string(), hash.to_string());
        self
    }

    pub fn build(self) -> FakeFilesystem {
        self.fs
    }
}

impl Filesystem for FakeFilesystem {
    async fn path_exists(&self, root: &str) -> bool {
        self.roots.contains(root)
    }

    async fn list_files(&self, root: &str) -> Result<Vec<FileEntry>, DomainError> {
        Ok(self
            .entries_by_root
            .get(root)
            .cloned()
            .unwrap_or_default())
    }

    async fn content_hash(&self, path: &str) -> Result<String, DomainError> {
        Ok(self
            .hash_by_path
            .get(path)
            .cloned()
            .unwrap_or_else(|| format!("hash-of-{path}")))
    }
}

/// Fixed clock wrapper for tests.
pub fn fixed_clock(now: DateTime<Utc>) -> FixedClock {
    FixedClock(now)
}

/// Convenience default timestamp used across index tests.
pub fn now() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap()
}

/// A single existing file used to seed `FakeCatalogRepository::with_existing`.
#[allow(dead_code)]
pub fn existing_file(path: &str, file_type: FileType) -> File {
    File {
        uuid: uuid::Uuid::new_v4(),
        path: path.to_string(),
        name: "seedy".to_string(),
        file_type,
        content_hash: "preexisting".to_string(),
        state: alexandria_core::catalog::model::FileState::Active,
        deleted_at: None,
        indexed_at: now(),
    }
}