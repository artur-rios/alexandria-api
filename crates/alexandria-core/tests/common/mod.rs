//! Shared test doubles for alexandria-core integration tests (Testing
//! Specification §6.2): hand-written fakes implementing the catalog
//! collaborators with no real database, filesystem, or auth service.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use uuid::Uuid;

use alexandria_core::auth::{AuthService, Principal};
use alexandria_core::catalog::clock::FixedClock;
use alexandria_core::catalog::fs::{FileEntry, Filesystem};
use alexandria_core::catalog::model::{File, FileState, FileType, NewFile, StateFilter, SubtypeMetadata};
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
    metadata: Arc<Mutex<HashMap<Uuid, SubtypeMetadata>>>,
    /// Paths whose `insert_file` / `refresh_hash` / `mark_missing` must fail,
    /// simulating a per-file repository error mid-run (UC-01 / UC-02: the run
    /// counts the failure and continues rather than aborting).
    failing_paths: Arc<Mutex<std::collections::HashSet<String>>>,
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

    /// File looked up by UUID — the UC-04 read path.
    pub fn file_for_uuid(&self, uuid: Uuid) -> Option<File> {
        self.files
            .lock()
            .unwrap()
            .values()
            .find(|f| f.uuid == uuid)
            .cloned()
    }

    /// Subtype metadata last written for `uuid` (UC-04 write path). `None`
    /// means no `update_metadata` call has persisted metadata for that file
    /// yet (the subtype row is still empty).
    pub fn metadata_for(&self, uuid: Uuid) -> Option<SubtypeMetadata> {
        self.metadata.lock().unwrap().get(&uuid).cloned()
    }

    /// Make every write against `path` fail with a database error, simulating
    /// a per-file repository failure part-way through a run.
    #[allow(dead_code)]
    pub fn failing_for(self, path: &str) -> Self {
        self.failing_paths.lock().unwrap().insert(path.to_string());
        self
    }

    fn fails(&self, path: &str) -> bool {
        self.failing_paths.lock().unwrap().contains(path)
    }
}

impl CatalogRepository for FakeCatalogRepository {
    async fn find_by_path(&self, path: &str) -> Result<Option<File>, DomainError> {
        Ok(self.files.lock().unwrap().get(path).cloned())
    }

    async fn find_by_uuid(&self, uuid: Uuid) -> Result<Option<File>, DomainError> {
        Ok(self.file_for_uuid(uuid))
    }

    async fn insert_file(&self, new_file: NewFile) -> Result<File, DomainError> {
        if self.fails(&new_file.path) {
            return Err(DomainError::internal("fake insert failure"));
        }
        let file = File {
            uuid: new_file.uuid,
            path: new_file.path.clone(),
            name: new_file.name,
            file_type: new_file.file_type,
            content_hash: new_file.content_hash,
            state: alexandria_core::catalog::model::FileState::Active,
            deleted_at: None,
            indexed_at: new_file.indexed_at,
            missing_at: None,
        };
        self.files.lock().unwrap().insert(new_file.path, file.clone());
        Ok(file)
    }

    async fn list_all(&self) -> Result<Vec<File>, DomainError> {
        let mut files: Vec<File> = self.files.lock().unwrap().values().cloned().collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(files)
    }

    async fn refresh_hash(
        &self,
        path: &str,
        content_hash: &str,
        indexed_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), DomainError> {
        if self.fails(path) {
            return Err(DomainError::internal("fake refresh failure"));
        }
        let mut files = self.files.lock().unwrap();
        if let Some(file) = files.get_mut(path) {
            file.content_hash = content_hash.to_string();
            file.indexed_at = indexed_at;
            file.missing_at = None;
        }
        Ok(())
    }

    async fn mark_missing(
        &self,
        path: &str,
        missing_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), DomainError> {
        if self.fails(path) {
            return Err(DomainError::internal("fake mark-missing failure"));
        }
        let mut files = self.files.lock().unwrap();
        if let Some(file) = files.get_mut(path) {
            file.missing_at = Some(missing_at);
        }
        Ok(())
    }

    async fn update_metadata(
        &self,
        uuid: Uuid,
        metadata: &SubtypeMetadata,
    ) -> Result<(), DomainError> {
        let files = self.files.lock().unwrap();
        let file = files
            .values()
            .find(|f| f.uuid == uuid)
            .ok_or(DomainError::NotFound)?;
        if file.file_type != metadata.file_type() {
            return Err(DomainError::InvalidInput(
                "metadata does not match file subtype".into(),
            ));
        }
        self.metadata
            .lock()
            .unwrap()
            .insert(uuid, metadata.clone());
        Ok(())
    }

    async fn list_filtered(
        &self,
        file_type: Option<FileType>,
        state: StateFilter,
    ) -> Result<Vec<File>, DomainError> {
        let files = self.files.lock().unwrap();
        let mut out: Vec<File> = files
            .values()
            .filter(|f| file_type.is_none() || Some(f.file_type) == file_type)
            .filter(|f| match state {
                StateFilter::Active => f.state == FileState::Active,
                StateFilter::Deleted => f.state == FileState::Deleted,
                StateFilter::All => true,
            })
            .cloned()
            .collect();
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    async fn find_metadata_by_uuid(
        &self,
        uuid: Uuid,
    ) -> Result<Option<SubtypeMetadata>, DomainError> {
        Ok(self.metadata.lock().unwrap().get(&uuid).cloned())
    }
}

/// In-memory filesystem. Stores which roots "exist" and a map of root -> the
/// entries `list_files` returns, plus a map of path -> content hash.
#[derive(Debug, Default)]
pub struct FakeFilesystem {
    roots: std::collections::HashSet<String>,
    entries_by_root: HashMap<String, Vec<FileEntry>>,
    hash_by_path: HashMap<String, String>,
    /// Paths that exist and are listed but cannot be read (locked / permission
    /// denied). `content_hash` fails for these, simulating the single bad file
    /// that must not abort a whole index or refresh run.
    unreadable: std::collections::HashSet<String>,
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

    /// A file that is present and listed but whose bytes cannot be read.
    #[allow(dead_code)]
    pub fn with_unreadable_file(mut self, root: &str, path: &str, name: &str) -> Self {
        self.fs.roots.insert(root.to_string());
        self.fs
            .entries_by_root
            .entry(root.to_string())
            .or_default()
            .push(FileEntry::new(path, name));
        self.fs.unreadable.insert(path.to_string());
        self
    }

    pub fn build(self) -> FakeFilesystem {
        self.fs
    }
}

impl Filesystem for FakeFilesystem {
    async fn path_exists(&self, root: &str) -> bool {
        // UC-01 calls with a root dir; UC-02 calls with a file path. A path is
        // "present" if it is either a registered root or a registered file.
        // An unreadable file is still present — it just cannot be hashed.
        self.roots.contains(root)
            || self.hash_by_path.contains_key(root)
            || self.unreadable.contains(root)
    }

    async fn list_files(&self, root: &str) -> Result<Vec<FileEntry>, DomainError> {
        Ok(self
            .entries_by_root
            .get(root)
            .cloned()
            .unwrap_or_default())
    }

    async fn content_hash(&self, path: &str) -> Result<String, DomainError> {
        if self.unreadable.contains(path) {
            return Err(DomainError::internal(format!("failed to read {path}")));
        }
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
        missing_at: None,
    }
}

/// A cataloged file in the `deleted` state (UC-04 AF-04 / UC-06). Used to
/// assert that editing metadata on a soft-deleted record is rejected with
/// `InvalidState` (restore first via UC-07).
#[allow(dead_code)]
pub fn deleted_file(path: &str, name: &str, file_type: FileType) -> File {
    File {
        uuid: uuid::Uuid::new_v4(),
        path: path.to_string(),
        name: name.to_string(),
        file_type,
        content_hash: "preexisting".to_string(),
        state: alexandria_core::catalog::model::FileState::Deleted,
        deleted_at: Some(earlier()),
        indexed_at: earlier(),
        missing_at: None,
    }
}

/// Existing cataloged file with a known hash (for UC-02 refresh tests).
#[allow(dead_code)]
pub fn existing_file_with_hash(path: &str, name: &str, file_type: FileType, hash: &str) -> File {
    File {
        uuid: uuid::Uuid::new_v4(),
        path: path.to_string(),
        name: name.to_string(),
        file_type,
        content_hash: hash.to_string(),
        state: alexandria_core::catalog::model::FileState::Active,
        deleted_at: None,
        indexed_at: earlier(),
        missing_at: None,
    }
}

/// A cataloged file already marked missing (the on-disk file was gone at a
/// prior re-index). Used to test the "file came back" path of UC-02.
#[allow(dead_code)]
pub fn existing_missing_file(path: &str, name: &str, file_type: FileType, hash: &str) -> File {
    File {
        uuid: uuid::Uuid::new_v4(),
        path: path.to_string(),
        name: name.to_string(),
        file_type,
        content_hash: hash.to_string(),
        state: alexandria_core::catalog::model::FileState::Active,
        deleted_at: None,
        indexed_at: earlier(),
        missing_at: Some(earlier()),
    }
}

/// Seed an arbitrary file directly into a fake repo (bypassing `insert_file`).
impl FakeCatalogRepository {
    pub fn seed(&self, file: File) {
        self.files.lock().unwrap().insert(file.path.clone(), file);
    }

    /// Seed stored subtype metadata for a file UUID (as if UC-04 had
    /// written it). Used by UC-03 read-path tests to assert the metadata
    /// appears in the returned `FileView`.
    pub fn seed_metadata(&self, uuid: Uuid, metadata: SubtypeMetadata) {
        self.metadata.lock().unwrap().insert(uuid, metadata);
    }
}

/// An "earlier" timestamp than `now()` so re-index refreshes `indexed_at`.
fn earlier() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(1_699_000_000, 0).unwrap()
}