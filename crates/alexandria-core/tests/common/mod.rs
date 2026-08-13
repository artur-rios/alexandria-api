//! Shared test doubles for alexandria-core integration tests (Testing
//! Specification §6.2): hand-written fakes implementing the catalog and
//! collections collaborators with no real database, filesystem, or auth
//! service.

// This module is included by more than one test target (`catalog.rs`,
// `collections.rs`), and each uses only the fakes for its own feature area —
// so every helper is dead code as far as the *other* target's compilation is
// concerned. The allow is module-wide rather than per item because the set of
// "unused here" helpers changes with every target that includes the file.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use uuid::Uuid;

use alexandria_core::auth::local::{LocalCredential, LocalCredentialRepository, SessionRepository};
use alexandria_core::auth::{AuthService, Principal};
use alexandria_core::bookmarks::model::{Bookmark, BookmarkState, NewBookmark};
use alexandria_core::bookmarks::repos::BookmarkRepository;
use alexandria_core::catalog::audio_tags::{AudioMetadataReader, AudioTags};
use alexandria_core::catalog::clock::FixedClock;
use alexandria_core::catalog::comic_tags::{ComicMetadataReader, ComicTags};
use alexandria_core::catalog::document_tags::{DocumentMetadataReader, DocumentTags};
use alexandria_core::catalog::fs::{FileEntry, Filesystem};
use alexandria_core::catalog::image_tags::{ImageMetadataReader, ImageTags};
use alexandria_core::catalog::model::{
    File, FileState, FileType, NewFile, StateFilter, SubtypeMetadata,
};
use alexandria_core::catalog::repos::CatalogRepository;
use alexandria_core::catalog::runs::{
    CatalogRun, CatalogRunRepository, RunCounts, RunKind, RunStatus,
};
use alexandria_core::catalog::video_tags::{VideoMetadataReader, VideoTags};
use alexandria_core::collections::model::{Collection, NewCollection};
use alexandria_core::collections::repos::CollectionRepository;
use alexandria_core::config::AuthMode;
use alexandria_core::errors::DomainError;
use alexandria_core::reading_lists::model::{
    NewReadingList, ReadingList, ReadingProgress, ReadingState, ReadingTargetKind,
};
use alexandria_core::reading_lists::repos::ReadingListRepository;
use alexandria_core::watchlists::model::{NewWatchlist, WatchProgress, WatchState, Watchlist};
use alexandria_core::watchlists::repos::WatchlistRepository;

/// Fake auth service. `Denying` rejects every caller (AF-02); `Allowing`
/// authenticates any token as the owner.
#[derive(Debug, Clone, Copy, Default)]
pub enum FakeAuth {
    #[default]
    Allowing,
    #[allow(dead_code)]
    Denying,
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
    /// UUIDs whose `rename_file` must fail with a Database error, simulating
    /// the post-rename catalog-failure branch of UC-05.
    failing_rename_file_uuids: Arc<Mutex<std::collections::HashSet<Uuid>>>,
    /// UUIDs whose `soft_delete` must fail, simulating a catalog-write failure
    /// in UC-06 (no on-disk leg to compensate — the handler surfaces the
    /// error and the catalog row is untouched because the fake never wrote
    /// it).
    failing_soft_delete_uuids: Arc<Mutex<std::collections::HashSet<Uuid>>>,
    /// UUIDs whose `restore` must fail, simulating a catalog-write failure in
    /// UC-07 (no on-disk leg to compensate — the handler surfaces the error
    /// and the catalog row is left `deleted` because the fake never wrote it).
    failing_restore_uuids: Arc<Mutex<std::collections::HashSet<Uuid>>>,
    /// UUIDs whose `purge` must fail, simulating a catalog-write failure in
    /// UC-08 (no on-disk leg to compensate — the handler surfaces the error
    /// and the catalog row is left `deleted` because the fake never removed
    /// it).
    failing_purge_uuids: Arc<Mutex<std::collections::HashSet<Uuid>>>,
    /// File uuid -> collection uuid, as written by `set_collection` (UC-13).
    collection_links: Arc<Mutex<HashMap<Uuid, Uuid>>>,
    /// File uuid -> (width, height), as written by `set_image_dimensions`
    /// (issue #44 image slice).
    dimensions: Arc<Mutex<HashMap<Uuid, (i64, i64)>>>,
    /// Page count last written for `uuid` via `set_document_page_count`
    /// (issue #44 document slice).
    document_page_counts: Arc<Mutex<HashMap<Uuid, i64>>>,
    /// Duration (seconds) last written for `uuid` via `set_video_duration`
    /// (issue #44 video slice).
    video_durations: Arc<Mutex<HashMap<Uuid, f64>>>,
    /// Page count last written for `uuid` via `set_comic_page_count`
    /// (issue #44 comic slice).
    comic_page_counts: Arc<Mutex<HashMap<Uuid, i64>>>,
}

impl FakeCatalogRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-seed an existing file at `path` so the indexer skips it (AF-03).
    pub fn with_existing(file: File) -> Self {
        let repo = Self::new();
        repo.files.lock().unwrap().insert(file.path.clone(), file);
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

    /// Make `rename_file` return a `Database` error for `uuid`, simulating the
    /// post-rename catalog-failure branch of UC-05 (the handler must roll the
    /// on-disk rename back). Used by the rename rollback unit test.
    pub fn fail_rename_file(&self, uuid: Uuid) {
        self.failing_rename_file_uuids.lock().unwrap().insert(uuid);
    }

    /// Make `soft_delete` return an `Internal` error for `uuid`, simulating a
    /// catalog-write failure in UC-06. There is no on-disk leg to compensate,
    /// so the handler merely surfaces the error and the row is left as-is.
    pub fn fail_soft_delete(&self, uuid: Uuid) {
        self.failing_soft_delete_uuids.lock().unwrap().insert(uuid);
    }

    /// Make `restore` return an `Internal` error for `uuid`, simulating a
    /// catalog-write failure in UC-07. As with `soft_delete` there is no
    /// on-disk leg to compensate, so the handler merely surfaces the error
    /// and the row stays `deleted`.
    pub fn fail_restore(&self, uuid: Uuid) {
        self.failing_restore_uuids.lock().unwrap().insert(uuid);
    }

    /// Make `purge` return an `Internal` error for `uuid`, simulating a
    /// catalog-write failure in UC-08. As with `restore` there is no on-disk
    /// leg to compensate, so the handler merely surfaces the error and the
    /// row stays present.
    pub fn fail_purge(&self, uuid: Uuid) {
        self.failing_purge_uuids.lock().unwrap().insert(uuid);
    }

    /// The collection uuid last linked to a file uuid via `set_collection`
    /// (UC-13). `None` when never linked.
    pub fn collection_for_file(&self, uuid: Uuid) -> Option<Uuid> {
        self.collection_links.lock().unwrap().get(&uuid).copied()
    }

    /// Dimensions last written for `uuid` via `set_image_dimensions`. `None`
    /// means no call has landed for that file yet.
    pub fn dimensions_for(&self, uuid: Uuid) -> Option<(i64, i64)> {
        self.dimensions.lock().unwrap().get(&uuid).copied()
    }

    /// Page count last written for `uuid` via `set_document_page_count`.
    /// `None` means no call has landed for that file yet.
    pub fn document_page_count_for(&self, uuid: Uuid) -> Option<i64> {
        self.document_page_counts
            .lock()
            .unwrap()
            .get(&uuid)
            .copied()
    }

    /// Duration (seconds) last written for `uuid` via `set_video_duration`.
    /// `None` means no call has landed for that file yet.
    pub fn video_duration_for(&self, uuid: Uuid) -> Option<f64> {
        self.video_durations.lock().unwrap().get(&uuid).copied()
    }

    /// Page count last written for `uuid` via `set_comic_page_count`.
    /// `None` means no call has landed for that file yet.
    pub fn comic_page_count_for(&self, uuid: Uuid) -> Option<i64> {
        self.comic_page_counts.lock().unwrap().get(&uuid).copied()
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
        self.files
            .lock()
            .unwrap()
            .insert(new_file.path, file.clone());
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
        self.metadata.lock().unwrap().insert(uuid, metadata.clone());
        Ok(())
    }

    async fn list_filtered(
        &self,
        file_type: Option<FileType>,
        state: StateFilter,
        collection_uuid: Option<Uuid>,
    ) -> Result<Vec<File>, DomainError> {
        let files = self.files.lock().unwrap();
        let links = self.collection_links.lock().unwrap();
        let mut out: Vec<File> = files
            .values()
            .filter(|f| file_type.is_none() || Some(f.file_type) == file_type)
            .filter(|f| match state {
                StateFilter::Active => f.state == FileState::Active,
                StateFilter::Deleted => f.state == FileState::Deleted,
                StateFilter::All => true,
            })
            .filter(|f| match collection_uuid {
                Some(c) => links.get(&f.uuid) == Some(&c),
                None => true,
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

    async fn set_image_dimensions(
        &self,
        uuid: Uuid,
        width: i64,
        height: i64,
    ) -> Result<(), DomainError> {
        let files = self.files.lock().unwrap();
        let file = files
            .values()
            .find(|f| f.uuid == uuid)
            .ok_or(DomainError::NotFound)?;
        if file.file_type != alexandria_core::catalog::model::FileType::Image {
            return Err(DomainError::InvalidInput("file is not an image".into()));
        }
        drop(files);
        self.dimensions
            .lock()
            .unwrap()
            .insert(uuid, (width, height));
        Ok(())
    }

    async fn find_image_dimensions(&self, uuid: Uuid) -> Result<Option<(i64, i64)>, DomainError> {
        let files = self.files.lock().unwrap();
        let file = match files.values().find(|f| f.uuid == uuid) {
            Some(f) => f,
            None => return Ok(None),
        };
        if file.file_type != alexandria_core::catalog::model::FileType::Image {
            return Ok(None);
        }
        drop(files);
        Ok(self.dimensions.lock().unwrap().get(&uuid).copied())
    }

    async fn set_document_page_count(
        &self,
        uuid: Uuid,
        page_count: i64,
    ) -> Result<(), DomainError> {
        let files = self.files.lock().unwrap();
        let file = files
            .values()
            .find(|f| f.uuid == uuid)
            .ok_or(DomainError::NotFound)?;
        if file.file_type != alexandria_core::catalog::model::FileType::Document {
            return Err(DomainError::InvalidInput("file is not a document".into()));
        }
        drop(files);
        self.document_page_counts
            .lock()
            .unwrap()
            .insert(uuid, page_count);
        Ok(())
    }

    async fn find_document_page_count(&self, uuid: Uuid) -> Result<Option<i64>, DomainError> {
        let files = self.files.lock().unwrap();
        let file = match files.values().find(|f| f.uuid == uuid) {
            Some(f) => f,
            None => return Ok(None),
        };
        if file.file_type != alexandria_core::catalog::model::FileType::Document {
            return Ok(None);
        }
        drop(files);
        Ok(self
            .document_page_counts
            .lock()
            .unwrap()
            .get(&uuid)
            .copied())
    }

    async fn set_video_duration(
        &self,
        uuid: Uuid,
        duration_seconds: f64,
    ) -> Result<(), DomainError> {
        let files = self.files.lock().unwrap();
        let file = files
            .values()
            .find(|f| f.uuid == uuid)
            .ok_or(DomainError::NotFound)?;
        if file.file_type != alexandria_core::catalog::model::FileType::Video {
            return Err(DomainError::InvalidInput("file is not a video".into()));
        }
        drop(files);
        self.video_durations
            .lock()
            .unwrap()
            .insert(uuid, duration_seconds);
        Ok(())
    }

    async fn find_video_duration(&self, uuid: Uuid) -> Result<Option<f64>, DomainError> {
        let files = self.files.lock().unwrap();
        let file = match files.values().find(|f| f.uuid == uuid) {
            Some(f) => f,
            None => return Ok(None),
        };
        if file.file_type != alexandria_core::catalog::model::FileType::Video {
            return Ok(None);
        }
        drop(files);
        Ok(self.video_durations.lock().unwrap().get(&uuid).copied())
    }

    async fn set_comic_page_count(&self, uuid: Uuid, page_count: i64) -> Result<(), DomainError> {
        let files = self.files.lock().unwrap();
        let file = files
            .values()
            .find(|f| f.uuid == uuid)
            .ok_or(DomainError::NotFound)?;
        if file.file_type != alexandria_core::catalog::model::FileType::Comic {
            return Err(DomainError::InvalidInput("file is not a comic".into()));
        }
        drop(files);
        self.comic_page_counts
            .lock()
            .unwrap()
            .insert(uuid, page_count);
        Ok(())
    }

    async fn find_comic_page_count(&self, uuid: Uuid) -> Result<Option<i64>, DomainError> {
        let files = self.files.lock().unwrap();
        let file = match files.values().find(|f| f.uuid == uuid) {
            Some(f) => f,
            None => return Ok(None),
        };
        if file.file_type != alexandria_core::catalog::model::FileType::Comic {
            return Ok(None);
        }
        drop(files);
        Ok(self.comic_page_counts.lock().unwrap().get(&uuid).copied())
    }

    async fn rename_file(
        &self,
        uuid: Uuid,
        new_name: &str,
        new_path: &str,
    ) -> Result<File, DomainError> {
        // Resolve the file by uuid, defending the unique-path invariant the
        // Sqlite impl relies on (a different file already owns `new_path`).
        let mut files = self.files.lock().unwrap();
        let file = files
            .values()
            .find(|f| f.uuid == uuid)
            .cloned()
            .ok_or(DomainError::NotFound)?;
        if self
            .failing_rename_file_uuids
            .lock()
            .unwrap()
            .contains(&uuid)
        {
            return Err(DomainError::internal("fake rename_file failure"));
        }
        if files.values().any(|f| f.path == new_path && f.uuid != uuid) {
            return Err(DomainError::InvalidInput(
                "target path already cataloged for a different file".into(),
            ));
        }
        let entry = files.get_mut(&file.path).expect("seeded file present");
        entry.name = new_name.to_string();
        entry.path = new_path.to_string();
        let renamed = entry.clone();
        // Re-key the map by path so subsequent find_by_path sees the new name.
        files.remove(&file.path);
        files.insert(new_path.to_string(), renamed.clone());
        Ok(renamed)
    }

    async fn soft_delete(
        &self,
        uuid: Uuid,
        deleted_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<File, DomainError> {
        let mut files = self.files.lock().unwrap();
        let file = files
            .values()
            .find(|f| f.uuid == uuid)
            .cloned()
            .ok_or(DomainError::NotFound)?;
        if self
            .failing_soft_delete_uuids
            .lock()
            .unwrap()
            .contains(&uuid)
        {
            return Err(DomainError::internal("fake soft_delete failure"));
        }
        let entry = files.get_mut(&file.path).expect("seeded file present");
        entry.state = FileState::Deleted;
        entry.deleted_at = Some(deleted_at);
        Ok(entry.clone())
    }

    async fn restore(&self, uuid: Uuid) -> Result<File, DomainError> {
        let mut files = self.files.lock().unwrap();
        let file = files
            .values()
            .find(|f| f.uuid == uuid)
            .cloned()
            .ok_or(DomainError::NotFound)?;
        if self.failing_restore_uuids.lock().unwrap().contains(&uuid) {
            return Err(DomainError::internal("fake restore failure"));
        }
        let entry = files.get_mut(&file.path).expect("seeded file present");
        entry.state = FileState::Active;
        entry.deleted_at = None;
        Ok(entry.clone())
    }

    async fn purge(&self, uuid: Uuid) -> Result<(), DomainError> {
        let mut files = self.files.lock().unwrap();
        let file = files
            .values()
            .find(|f| f.uuid == uuid)
            .cloned()
            .ok_or(DomainError::NotFound)?;
        if self.failing_purge_uuids.lock().unwrap().contains(&uuid) {
            return Err(DomainError::internal("fake purge failure"));
        }
        files.remove(&file.path);
        Ok(())
    }

    async fn set_collection(&self, uuid: Uuid, collection_uuid: Uuid) -> Result<(), DomainError> {
        if self.file_for_uuid(uuid).is_none() {
            return Err(DomainError::NotFound);
        }
        self.collection_links
            .lock()
            .unwrap()
            .insert(uuid, collection_uuid);
        Ok(())
    }

    async fn clear_collection(&self, uuid: Uuid, collection_uuid: Uuid) -> Result<(), DomainError> {
        let mut links = self.collection_links.lock().unwrap();
        if links.get(&uuid) != Some(&collection_uuid) {
            return Err(DomainError::NotFound);
        }
        links.remove(&uuid);
        Ok(())
    }
}

/// In-memory collections repository (UC-10). Backed by a shared
/// `Arc<Mutex<…>>` so a test can clone the repo, hand the original to the
/// handler, and inspect the clone afterwards — the same arrangement
/// `FakeCatalogRepository` uses.
#[derive(Debug, Default, Clone)]
pub struct FakeCollectionRepository {
    collections: Arc<Mutex<HashMap<Uuid, Collection>>>,
    /// When set, every `insert_collection` fails, simulating a catalog-write
    /// failure in UC-10. There is no on-disk leg to compensate — the handler
    /// merely surfaces the error and nothing is stored.
    failing: Arc<Mutex<bool>>,
    /// When set, every `rename_collection` fails, simulating a catalog-write
    /// failure in UC-11.
    failing_renames: Arc<Mutex<bool>>,
    /// When set, every `delete_collection` fails, simulating a catalog-write
    /// failure in UC-12.
    failing_deletes: Arc<Mutex<bool>>,
}

impl FakeCollectionRepository {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn count(&self) -> usize {
        self.collections.lock().unwrap().len()
    }

    pub fn collection_for(&self, uuid: Uuid) -> Option<Collection> {
        self.collections.lock().unwrap().get(&uuid).cloned()
    }

    /// Seed a collection directly, as if a prior UC-10 call had created it.
    pub fn seed(&self, collection: Collection) {
        self.collections
            .lock()
            .unwrap()
            .insert(collection.uuid, collection);
    }

    /// Make every `insert_collection` return an `Internal` error.
    pub fn fail_inserts(&self) {
        *self.failing.lock().unwrap() = true;
    }

    /// Make every `rename_collection` return an `Internal` error.
    pub fn fail_renames(&self) {
        *self.failing_renames.lock().unwrap() = true;
    }

    /// Make every `delete_collection` return an `Internal` error.
    pub fn fail_deletes(&self) {
        *self.failing_deletes.lock().unwrap() = true;
    }
}

impl CollectionRepository for FakeCollectionRepository {
    async fn insert_collection(
        &self,
        new_collection: NewCollection,
    ) -> Result<Collection, DomainError> {
        if *self.failing.lock().unwrap() {
            return Err(DomainError::internal("fake insert_collection failure"));
        }
        let collection = Collection {
            uuid: new_collection.uuid,
            name: new_collection.name,
            kind: new_collection.kind,
        };
        self.collections
            .lock()
            .unwrap()
            .insert(collection.uuid, collection.clone());
        Ok(collection)
    }

    async fn find_by_uuid(&self, uuid: Uuid) -> Result<Option<Collection>, DomainError> {
        Ok(self.collections.lock().unwrap().get(&uuid).cloned())
    }

    async fn rename_collection(&self, uuid: Uuid, name: String) -> Result<Collection, DomainError> {
        if *self.failing_renames.lock().unwrap() {
            return Err(DomainError::internal("fake rename_collection failure"));
        }
        let mut collections = self.collections.lock().unwrap();
        let collection = collections.get_mut(&uuid).ok_or(DomainError::NotFound)?;
        collection.name = name;
        Ok(collection.clone())
    }

    async fn delete_collection(&self, uuid: Uuid) -> Result<(), DomainError> {
        if *self.failing_deletes.lock().unwrap() {
            return Err(DomainError::internal("fake delete_collection failure"));
        }
        self.collections.lock().unwrap().remove(&uuid);
        Ok(())
    }
}

/// In-memory filesystem. Stores which roots "exist" and a map of root -> the
/// entries `list_files` returns, plus a map of path -> content hash.
#[derive(Debug, Default, Clone)]
pub struct FakeFilesystem {
    roots: std::collections::HashSet<String>,
    entries_by_root: HashMap<String, Vec<FileEntry>>,
    hash_by_path: HashMap<String, String>,
    /// Paths that exist and are listed but cannot be read (locked / permission
    /// denied). `content_hash` fails for these, simulating the single bad file
    /// that must not abort a whole index or refresh run.
    unreadable: std::collections::HashSet<String>,
    /// Text content readable via `read_file` (UC-32), keyed by path.
    content_by_path: HashMap<String, String>,
    /// Interior-mutable post-construction state for the `rename` port
    /// (UC-05): the trait takes `&self`, so a rename that would move the
    /// recorded hash and update `path_exists` must do so through a lock.
    state: Arc<Mutex<FakeFsState>>,
}

#[derive(Debug, Default)]
struct FakeFsState {
    /// Paths where `rename` must fail, simulating UC-05 AF-02.
    failing_renames_from: std::collections::HashSet<String>,
    /// Paths recorded as "exists on disk" — simulates an on-disk entry not in
    /// the catalog so the rename handler's target-exists guard fires.
    disk_paths: std::collections::HashSet<String>,
    /// Completed renames `from -> to`, in order. `path_exists` reports the
    /// `to` path as present (and the `from` path as gone) after a rename.
    renames: Vec<(String, String)>,
    /// Paths where `remove_file` must fail, simulating UC-09 AF-02.
    failing_removes_from: std::collections::HashSet<String>,
    /// Paths removed by `remove_file` so far, in order. `path_exists` reports
    /// these as gone afterwards.
    removed: Vec<String>,
    /// Paths where `write_file` must fail, simulating UC-33 AF-02.
    failing_writes_from: std::collections::HashSet<String>,
    /// Paths where `write_file` "succeeds" but the bytes actually stored
    /// differ from what was submitted, simulating UC-33 AF-03 (the
    /// post-write hash does not match).
    corrupt_writes_from: std::collections::HashSet<String>,
    /// Content actually stored by `write_file` so far, keyed by path.
    /// `read_file` and `content_hash` prefer this over the builder-seeded
    /// `content_by_path`/`hash_by_path` once a write has happened.
    written: HashMap<String, String>,
}

impl FakeFilesystem {
    pub fn builder() -> FakeFilesystemBuilder {
        FakeFilesystemBuilder::default()
    }

    /// Make `rename` from `path` fail with a disk error (UC-05 AF-02).
    pub fn fail_rename_from(&mut self, path: &str) {
        self.state
            .lock()
            .unwrap()
            .failing_renames_from
            .insert(path.to_string());
    }

    /// Record an on-disk entry at `path` (not cataloged) so the rename
    /// handler's target-exists-on-disk guard fires (UC-05 AF-02).
    pub fn place_disk_file(&mut self, path: &str) {
        self.state
            .lock()
            .unwrap()
            .disk_paths
            .insert(path.to_string());
    }

    /// `true` once a rename `from -> to` has been recorded by the fake.
    pub fn renamed_to(&self, from: &str, to: &str) -> bool {
        self.state
            .lock()
            .unwrap()
            .renames
            .iter()
            .any(|(f, t)| f == from && t == to)
    }

    /// Count of renames the fake has performed so far.
    pub fn rename_count(&self) -> usize {
        self.state.lock().unwrap().renames.len()
    }

    /// Make `write_file` at `path` fail with a disk error (UC-33 AF-02).
    #[allow(dead_code)]
    pub fn fail_write_to(&mut self, path: &str) {
        self.state
            .lock()
            .unwrap()
            .failing_writes_from
            .insert(path.to_string());
    }

    /// Make `write_file` at `path` silently store different bytes than
    /// submitted, so the post-write hash never matches (UC-33 AF-03).
    #[allow(dead_code)]
    pub fn corrupt_write_to(&mut self, path: &str) {
        self.state
            .lock()
            .unwrap()
            .corrupt_writes_from
            .insert(path.to_string());
    }

    /// The bytes `write_file` most recently stored at `path`, if any.
    #[allow(dead_code)]
    pub fn written_content(&self, path: &str) -> Option<String> {
        self.state.lock().unwrap().written.get(path).cloned()
    }

    /// Make `remove_file` at `path` fail with a disk error (UC-09 AF-02).
    pub fn fail_remove_from(&mut self, path: &str) {
        self.state
            .lock()
            .unwrap()
            .failing_removes_from
            .insert(path.to_string());
    }

    /// `true` once `remove_file` has removed `path`.
    pub fn removed(&self, path: &str) -> bool {
        self.state.lock().unwrap().removed.iter().any(|p| p == path)
    }

    /// Count of successful removals the fake has performed so far.
    pub fn remove_count(&self) -> usize {
        self.state.lock().unwrap().removed.len()
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
        self.fs
            .hash_by_path
            .insert(path.to_string(), hash.to_string());
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

    /// A file whose `read_file` (UC-32) returns `content`.
    #[allow(dead_code)]
    pub fn with_text_content(mut self, path: &str, content: &str) -> Self {
        self.fs
            .content_by_path
            .insert(path.to_string(), content.to_string());
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
        // After a rename, the `to` path is present and the `from` path is gone.
        let state = self.state.lock().unwrap();
        let moved_from = state.renames.iter().any(|(f, _)| f == root)
            && !state.renames.iter().any(|(_, t)| t == root);
        let moved_to = state.renames.iter().any(|(_, t)| t == root);
        let disk = state.disk_paths.contains(root);
        let removed = state.removed.iter().any(|p| p == root);
        drop(state);
        !removed
            && (self.roots.contains(root)
                || self.hash_by_path.contains_key(root)
                || self.unreadable.contains(root)
                || disk
                || (moved_to && !moved_from))
    }

    async fn list_files(&self, root: &str) -> Result<Vec<FileEntry>, DomainError> {
        Ok(self.entries_by_root.get(root).cloned().unwrap_or_default())
    }

    async fn content_hash(&self, path: &str) -> Result<String, DomainError> {
        if self.unreadable.contains(path) {
            return Err(DomainError::internal(format!("failed to read {path}")));
        }
        if let Some(written) = self.state.lock().unwrap().written.get(path) {
            return Ok(alexandria_core::catalog::fs::sha256_hex(written.as_bytes()));
        }
        Ok(self
            .hash_by_path
            .get(path)
            .cloned()
            .unwrap_or_else(|| format!("hash-of-{path}")))
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), DomainError> {
        let mut state = self.state.lock().unwrap();
        if state.failing_renames_from.contains(from) {
            return Err(DomainError::disk(format!(
                "fake rename failure: {from:?} -> {to:?}"
            )));
        }
        state.renames.push((from.to_string(), to.to_string()));
        Ok(())
    }

    async fn remove_file(&self, path: &str) -> Result<bool, DomainError> {
        let mut state = self.state.lock().unwrap();
        if state.failing_removes_from.contains(path) {
            return Err(DomainError::disk(format!("fake remove failure: {path:?}")));
        }
        if state.removed.iter().any(|p| p == path) {
            return Ok(false);
        }
        let present = self.roots.contains(path)
            || self.hash_by_path.contains_key(path)
            || self.unreadable.contains(path)
            || state.disk_paths.contains(path);
        if present {
            state.removed.push(path.to_string());
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn read_file(&self, path: &str) -> Result<String, DomainError> {
        if self.unreadable.contains(path) {
            return Err(DomainError::disk(format!("fake read failure: {path}")));
        }
        if let Some(written) = self.state.lock().unwrap().written.get(path) {
            return Ok(written.clone());
        }
        self.content_by_path
            .get(path)
            .cloned()
            .ok_or_else(|| DomainError::disk(format!("fake file not found: {path}")))
    }

    async fn write_file(&self, path: &str, content: &str) -> Result<(), DomainError> {
        let mut state = self.state.lock().unwrap();
        if state.failing_writes_from.contains(path) {
            return Err(DomainError::disk(format!("fake write failure: {path:?}")));
        }
        let stored = if state.corrupt_writes_from.contains(path) {
            format!("{content}-corrupted")
        } else {
            content.to_string()
        };
        state.written.insert(path.to_string(), stored);
        Ok(())
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
/// `InvalidState` (restore first via UC-07). The `deleted_at` stamp is
/// `earlier()` (~115 days before `now()`), so this helper is **past** the
/// default 30-day retention window and is the fixture for the AF-01
/// "past-retention → NotFound" branch of UC-07.
#[allow(dead_code)]
pub fn deleted_file(path: &str, name: &str, file_type: FileType) -> File {
    deleted_file_at(path, name, file_type, earlier())
}

/// A cataloged file in the `deleted` state with an explicit `deleted_at`,
/// so UC-07 retention-window tests can place the row precisely relative to
/// `now()` (within-retention, exactly on the boundary, or past it).
#[allow(dead_code)]
pub fn deleted_file_at(
    path: &str,
    name: &str,
    file_type: FileType,
    deleted_at: DateTime<Utc>,
) -> File {
    File {
        uuid: uuid::Uuid::new_v4(),
        path: path.to_string(),
        name: name.to_string(),
        file_type,
        content_hash: "preexisting".to_string(),
        state: alexandria_core::catalog::model::FileState::Deleted,
        deleted_at: Some(deleted_at),
        indexed_at: deleted_at,
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

/// In-memory bookmarks repository (UC-15). Backed by a shared
/// `Arc<Mutex<…>>` so a test can clone the repo, hand the original to the
/// handler, and inspect the clone afterwards — the same arrangement
/// `FakeCollectionRepository` uses.
#[derive(Debug, Default, Clone)]
pub struct FakeBookmarkRepository {
    bookmarks: Arc<Mutex<HashMap<Uuid, Bookmark>>>,
    /// When set, every `insert_bookmark` fails, simulating a catalog-write
    /// failure in UC-15. There is no on-disk leg to compensate — the handler
    /// merely surfaces the error and nothing is stored.
    failing: Arc<Mutex<bool>>,
    /// When set, every `update_bookmark` fails, simulating a catalog-write
    /// failure in UC-16.
    failing_updates: Arc<Mutex<bool>>,
}

impl FakeBookmarkRepository {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn count(&self) -> usize {
        self.bookmarks.lock().unwrap().len()
    }

    pub fn bookmark_for(&self, uuid: Uuid) -> Option<Bookmark> {
        self.bookmarks.lock().unwrap().get(&uuid).cloned()
    }

    /// Seed a bookmark directly, as if a prior UC-15 call had created it.
    pub fn seed(&self, bookmark: Bookmark) {
        self.bookmarks
            .lock()
            .unwrap()
            .insert(bookmark.uuid, bookmark);
    }

    /// Make every `insert_bookmark` return an `Internal` error.
    pub fn fail_inserts(&self) {
        *self.failing.lock().unwrap() = true;
    }

    /// Make every `update_bookmark` return an `Internal` error.
    pub fn fail_updates(&self) {
        *self.failing_updates.lock().unwrap() = true;
    }
}

impl BookmarkRepository for FakeBookmarkRepository {
    async fn insert_bookmark(&self, new_bookmark: NewBookmark) -> Result<Bookmark, DomainError> {
        if *self.failing.lock().unwrap() {
            return Err(DomainError::internal("fake insert_bookmark failure"));
        }
        let bookmark = Bookmark {
            uuid: new_bookmark.uuid,
            url: new_bookmark.url,
            title: new_bookmark.title,
            state: BookmarkState::Active,
            deleted_at: None,
            collection_uuid: new_bookmark.collection_uuid,
        };
        self.bookmarks
            .lock()
            .unwrap()
            .insert(bookmark.uuid, bookmark.clone());
        Ok(bookmark)
    }

    async fn find_by_uuid(&self, uuid: Uuid) -> Result<Option<Bookmark>, DomainError> {
        Ok(self.bookmarks.lock().unwrap().get(&uuid).cloned())
    }

    async fn set_collection(&self, uuid: Uuid, collection_uuid: Uuid) -> Result<(), DomainError> {
        let mut bookmarks = self.bookmarks.lock().unwrap();
        let bookmark = bookmarks.get_mut(&uuid).ok_or(DomainError::NotFound)?;
        bookmark.collection_uuid = Some(collection_uuid);
        Ok(())
    }

    async fn clear_collection(&self, uuid: Uuid, collection_uuid: Uuid) -> Result<(), DomainError> {
        let mut bookmarks = self.bookmarks.lock().unwrap();
        let bookmark = bookmarks.get_mut(&uuid).ok_or(DomainError::NotFound)?;
        if bookmark.collection_uuid != Some(collection_uuid) {
            return Err(DomainError::NotFound);
        }
        bookmark.collection_uuid = None;
        Ok(())
    }

    async fn list_filtered(
        &self,
        collection_uuid: Option<Uuid>,
        state: StateFilter,
    ) -> Result<Vec<Bookmark>, DomainError> {
        let bookmarks = self.bookmarks.lock().unwrap();
        let mut out: Vec<Bookmark> = bookmarks
            .values()
            .filter(|b| collection_uuid.is_none() || b.collection_uuid == collection_uuid)
            .filter(|b| match state {
                StateFilter::Active => b.state == BookmarkState::Active,
                StateFilter::Deleted => b.state == BookmarkState::Deleted,
                StateFilter::All => true,
            })
            .cloned()
            .collect();
        out.sort_by(|a, b| a.title.cmp(&b.title));
        Ok(out)
    }

    async fn update_bookmark(
        &self,
        uuid: Uuid,
        url: String,
        title: String,
        collection_uuid: Option<Uuid>,
    ) -> Result<Bookmark, DomainError> {
        if *self.failing_updates.lock().unwrap() {
            return Err(DomainError::internal("fake update_bookmark failure"));
        }
        let mut bookmarks = self.bookmarks.lock().unwrap();
        let bookmark = bookmarks.get_mut(&uuid).ok_or(DomainError::NotFound)?;
        bookmark.url = url;
        bookmark.title = title;
        bookmark.collection_uuid = collection_uuid;
        Ok(bookmark.clone())
    }

    async fn soft_delete(
        &self,
        uuid: Uuid,
        deleted_at: DateTime<Utc>,
    ) -> Result<Bookmark, DomainError> {
        let mut bookmarks = self.bookmarks.lock().unwrap();
        let bookmark = bookmarks.get_mut(&uuid).ok_or(DomainError::NotFound)?;
        bookmark.state = BookmarkState::Deleted;
        bookmark.deleted_at = Some(deleted_at);
        Ok(bookmark.clone())
    }

    async fn restore(&self, uuid: Uuid) -> Result<Bookmark, DomainError> {
        let mut bookmarks = self.bookmarks.lock().unwrap();
        let bookmark = bookmarks.get_mut(&uuid).ok_or(DomainError::NotFound)?;
        bookmark.state = BookmarkState::Active;
        bookmark.deleted_at = None;
        Ok(bookmark.clone())
    }

    async fn purge(&self, uuid: Uuid) -> Result<(), DomainError> {
        let mut bookmarks = self.bookmarks.lock().unwrap();
        bookmarks.remove(&uuid).ok_or(DomainError::NotFound)?;
        Ok(())
    }
}

/// In-memory watchlists repository (UC-20).
#[derive(Debug, Default, Clone)]
pub struct FakeWatchlistRepository {
    watchlists: Arc<Mutex<HashMap<Uuid, Watchlist>>>,
    /// When set, every `insert_watchlist` fails, simulating a catalog-write
    /// failure in UC-20.
    failing: Arc<Mutex<bool>>,
    progress: Arc<Mutex<HashMap<(Uuid, Uuid), WatchProgress>>>,
}

impl FakeWatchlistRepository {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn count(&self) -> usize {
        self.watchlists.lock().unwrap().len()
    }

    pub fn watchlist_for(&self, uuid: Uuid) -> Option<Watchlist> {
        self.watchlists.lock().unwrap().get(&uuid).cloned()
    }

    /// Make every `insert_watchlist` return an `Internal` error.
    pub fn fail_inserts(&self) {
        *self.failing.lock().unwrap() = true;
    }
}

impl WatchlistRepository for FakeWatchlistRepository {
    async fn insert_watchlist(
        &self,
        new_watchlist: NewWatchlist,
    ) -> Result<Watchlist, DomainError> {
        if *self.failing.lock().unwrap() {
            return Err(DomainError::internal("fake insert_watchlist failure"));
        }
        let watchlist = Watchlist {
            uuid: new_watchlist.uuid,
            name: new_watchlist.name,
        };
        self.watchlists
            .lock()
            .unwrap()
            .insert(watchlist.uuid, watchlist.clone());
        Ok(watchlist)
    }

    async fn find_by_uuid(&self, uuid: Uuid) -> Result<Option<Watchlist>, DomainError> {
        Ok(self.watchlists.lock().unwrap().get(&uuid).cloned())
    }

    async fn list_all(&self) -> Result<Vec<Watchlist>, DomainError> {
        let mut watchlists: Vec<Watchlist> =
            self.watchlists.lock().unwrap().values().cloned().collect();
        watchlists.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(watchlists)
    }

    async fn list_progress(&self, watchlist_uuid: Uuid) -> Result<Vec<WatchProgress>, DomainError> {
        let mut items: Vec<WatchProgress> = self
            .progress
            .lock()
            .unwrap()
            .values()
            .filter(|p| p.watchlist_uuid == watchlist_uuid)
            .cloned()
            .collect();
        items.sort_by_key(|a| a.video_uuid);
        Ok(items)
    }

    async fn add_video(
        &self,
        watchlist_uuid: Uuid,
        video_uuid: Uuid,
    ) -> Result<WatchProgress, DomainError> {
        let mut progress = self.progress.lock().unwrap();
        let entry = progress
            .entry((watchlist_uuid, video_uuid))
            .or_insert_with(|| WatchProgress {
                watchlist_uuid,
                video_uuid,
                state: WatchState::Pending,
                current_episode: None,
                total_episodes: None,
            });
        Ok(entry.clone())
    }

    async fn find_progress(
        &self,
        watchlist_uuid: Uuid,
        video_uuid: Uuid,
    ) -> Result<Option<WatchProgress>, DomainError> {
        Ok(self
            .progress
            .lock()
            .unwrap()
            .get(&(watchlist_uuid, video_uuid))
            .cloned())
    }

    async fn update_progress(
        &self,
        watchlist_uuid: Uuid,
        video_uuid: Uuid,
        state: WatchState,
        current_episode: Option<i64>,
        total_episodes: Option<i64>,
    ) -> Result<WatchProgress, DomainError> {
        let updated = WatchProgress {
            watchlist_uuid,
            video_uuid,
            state,
            current_episode,
            total_episodes,
        };
        self.progress
            .lock()
            .unwrap()
            .insert((watchlist_uuid, video_uuid), updated.clone());
        Ok(updated)
    }

    async fn remove_progress(
        &self,
        watchlist_uuid: Uuid,
        video_uuid: Uuid,
    ) -> Result<(), DomainError> {
        let removed = self
            .progress
            .lock()
            .unwrap()
            .remove(&(watchlist_uuid, video_uuid));
        match removed {
            Some(_) => Ok(()),
            None => Err(DomainError::NotFound),
        }
    }

    async fn delete_watchlist(&self, uuid: Uuid) -> Result<(), DomainError> {
        self.watchlists.lock().unwrap().remove(&uuid);
        self.progress
            .lock()
            .unwrap()
            .retain(|(watchlist_uuid, _), _| *watchlist_uuid != uuid);
        Ok(())
    }
}

/// In-memory reading lists repository (UC-26).
#[derive(Debug, Default, Clone)]
pub struct FakeReadingListRepository {
    reading_lists: Arc<Mutex<HashMap<Uuid, ReadingList>>>,
    /// When set, every `insert_reading_list` fails, simulating a
    /// catalog-write failure in UC-26.
    failing: Arc<Mutex<bool>>,
    progress: Arc<Mutex<HashMap<(Uuid, Uuid), ReadingProgress>>>,
}

impl FakeReadingListRepository {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn count(&self) -> usize {
        self.reading_lists.lock().unwrap().len()
    }

    pub fn reading_list_for(&self, uuid: Uuid) -> Option<ReadingList> {
        self.reading_lists.lock().unwrap().get(&uuid).cloned()
    }

    /// Make every `insert_reading_list` return an `Internal` error.
    pub fn fail_inserts(&self) {
        *self.failing.lock().unwrap() = true;
    }
}

impl ReadingListRepository for FakeReadingListRepository {
    async fn insert_reading_list(
        &self,
        new_reading_list: NewReadingList,
    ) -> Result<ReadingList, DomainError> {
        if *self.failing.lock().unwrap() {
            return Err(DomainError::internal("fake insert_reading_list failure"));
        }
        let reading_list = ReadingList {
            uuid: new_reading_list.uuid,
            name: new_reading_list.name,
        };
        self.reading_lists
            .lock()
            .unwrap()
            .insert(reading_list.uuid, reading_list.clone());
        Ok(reading_list)
    }

    async fn find_by_uuid(&self, uuid: Uuid) -> Result<Option<ReadingList>, DomainError> {
        Ok(self.reading_lists.lock().unwrap().get(&uuid).cloned())
    }

    async fn list_all(&self) -> Result<Vec<ReadingList>, DomainError> {
        let mut reading_lists: Vec<ReadingList> = self
            .reading_lists
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect();
        reading_lists.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(reading_lists)
    }

    async fn list_progress(
        &self,
        reading_list_uuid: Uuid,
    ) -> Result<Vec<ReadingProgress>, DomainError> {
        let mut items: Vec<ReadingProgress> = self
            .progress
            .lock()
            .unwrap()
            .values()
            .filter(|p| p.reading_list_uuid == reading_list_uuid)
            .cloned()
            .collect();
        items.sort_by_key(|a| a.item_uuid);
        Ok(items)
    }

    async fn add_item(
        &self,
        reading_list_uuid: Uuid,
        item_uuid: Uuid,
        target_kind: ReadingTargetKind,
    ) -> Result<ReadingProgress, DomainError> {
        let mut progress = self.progress.lock().unwrap();
        let entry = progress
            .entry((reading_list_uuid, item_uuid))
            .or_insert_with(|| ReadingProgress {
                reading_list_uuid,
                item_uuid,
                target_kind,
                state: ReadingState::Pending,
                current_issue: None,
                total_issues: None,
            });
        Ok(entry.clone())
    }

    async fn find_progress(
        &self,
        reading_list_uuid: Uuid,
        item_uuid: Uuid,
    ) -> Result<Option<ReadingProgress>, DomainError> {
        Ok(self
            .progress
            .lock()
            .unwrap()
            .get(&(reading_list_uuid, item_uuid))
            .cloned())
    }

    async fn update_progress(
        &self,
        reading_list_uuid: Uuid,
        item_uuid: Uuid,
        state: ReadingState,
        current_issue: Option<i64>,
        total_issues: Option<i64>,
    ) -> Result<ReadingProgress, DomainError> {
        let mut progress = self.progress.lock().unwrap();
        let existing = progress
            .get(&(reading_list_uuid, item_uuid))
            .ok_or(DomainError::NotFound)?;
        let updated = ReadingProgress {
            reading_list_uuid,
            item_uuid,
            target_kind: existing.target_kind,
            state,
            current_issue,
            total_issues,
        };
        progress.insert((reading_list_uuid, item_uuid), updated.clone());
        Ok(updated)
    }

    async fn remove_progress(
        &self,
        reading_list_uuid: Uuid,
        item_uuid: Uuid,
    ) -> Result<(), DomainError> {
        let removed = self
            .progress
            .lock()
            .unwrap()
            .remove(&(reading_list_uuid, item_uuid));
        match removed {
            Some(_) => Ok(()),
            None => Err(DomainError::NotFound),
        }
    }

    async fn delete_reading_list(&self, uuid: Uuid) -> Result<(), DomainError> {
        self.reading_lists.lock().unwrap().remove(&uuid);
        self.progress
            .lock()
            .unwrap()
            .retain(|(reading_list_uuid, _), _| *reading_list_uuid != uuid);
        Ok(())
    }
}

/// In-memory local-login credentials repository (UC-34/UC-35). Starts with no
/// row — `get()` answers `None` until `upsert` is called, mirroring
/// first-time setup.
#[derive(Debug, Default, Clone)]
pub struct FakeLocalCredentialRepository {
    credential: Arc<Mutex<Option<LocalCredential>>>,
}

impl FakeLocalCredentialRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed a credential row directly, as if a prior UC-35 call had set it.
    pub fn seed(&self, email: &str, password_hash: &str) {
        *self.credential.lock().unwrap() = Some(LocalCredential {
            email: email.to_string(),
            password_hash: password_hash.to_string(),
        });
    }
}

impl LocalCredentialRepository for FakeLocalCredentialRepository {
    async fn get(&self) -> Result<Option<LocalCredential>, DomainError> {
        Ok(self.credential.lock().unwrap().clone())
    }

    async fn upsert(
        &self,
        email: &str,
        password_hash: &str,
        _updated_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        *self.credential.lock().unwrap() = Some(LocalCredential {
            email: email.to_string(),
            password_hash: password_hash.to_string(),
        });
        Ok(())
    }

    async fn insert_if_absent(
        &self,
        email: &str,
        password_hash: &str,
        _updated_at: DateTime<Utc>,
    ) -> Result<bool, DomainError> {
        let mut guard = self.credential.lock().unwrap();
        if guard.is_some() {
            return Ok(false);
        }
        *guard = Some(LocalCredential {
            email: email.to_string(),
            password_hash: password_hash.to_string(),
        });
        Ok(true)
    }
}

/// A `LocalCredentialRepository` that simulates a row appearing between the
/// existence check and the write (UC-41 fix for the check-then-act race):
/// `get()` always answers `None`, as if the check ran before the
/// concurrent write landed, while `insert_if_absent` sees the row that is
/// "already there" at the storage layer and refuses to overwrite it. Lets a
/// test drive the race without two real concurrent tasks.
#[derive(Debug, Clone)]
pub struct RacingLocalCredentialRepository {
    credential: Arc<Mutex<LocalCredential>>,
}

impl RacingLocalCredentialRepository {
    /// Pre-seed the row that "wins" the race.
    pub fn new(email: &str, password_hash: &str) -> Self {
        Self {
            credential: Arc::new(Mutex::new(LocalCredential {
                email: email.to_string(),
                password_hash: password_hash.to_string(),
            })),
        }
    }

    pub fn stored(&self) -> LocalCredential {
        self.credential.lock().unwrap().clone()
    }
}

impl LocalCredentialRepository for RacingLocalCredentialRepository {
    async fn get(&self) -> Result<Option<LocalCredential>, DomainError> {
        // Simulates the existence check having run before the concurrent
        // write landed.
        Ok(None)
    }

    async fn upsert(
        &self,
        email: &str,
        password_hash: &str,
        _updated_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        *self.credential.lock().unwrap() = LocalCredential {
            email: email.to_string(),
            password_hash: password_hash.to_string(),
        };
        Ok(())
    }

    async fn insert_if_absent(
        &self,
        _email: &str,
        _password_hash: &str,
        _updated_at: DateTime<Utc>,
    ) -> Result<bool, DomainError> {
        // The row is already there at the storage layer, regardless of what
        // `get()` answered — this is the whole point of the fake.
        Ok(false)
    }
}

/// In-memory session repository (UC-34's postcondition: "a session must be
/// created to keep track of the login").
#[derive(Debug, Default, Clone)]
pub struct FakeSessionRepository {
    sessions: Arc<Mutex<HashMap<Uuid, DateTime<Utc>>>>,
}

impl FakeSessionRepository {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn count(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }
}

impl SessionRepository for FakeSessionRepository {
    async fn create_session(
        &self,
        id: Uuid,
        _created_at: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        self.sessions.lock().unwrap().insert(id, expires_at);
        Ok(())
    }

    async fn is_valid(&self, id: Uuid, now: DateTime<Utc>) -> Result<bool, DomainError> {
        Ok(self
            .sessions
            .lock()
            .unwrap()
            .get(&id)
            .is_some_and(|expires_at| now < *expires_at))
    }
}

/// A `SessionRepository` whose writes always fail (UC-41 AF-06). Lets a
/// test drive the "credential row written, session creation failed" path
/// without a real database.
#[derive(Debug, Default, Clone, Copy)]
pub struct FailingSessionRepository;

impl SessionRepository for FailingSessionRepository {
    async fn create_session(
        &self,
        _id: Uuid,
        _created_at: DateTime<Utc>,
        _expires_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        Err(DomainError::Disk("session store unavailable".into()))
    }

    async fn is_valid(&self, _id: Uuid, _now: DateTime<Utc>) -> Result<bool, DomainError> {
        Err(DomainError::Disk("session store unavailable".into()))
    }
}

/// In-memory audio-tag reader (issue #44 pilot). `read()` answers `None`
/// for any path with no seeded tags, mirroring "no tags found / couldn't
/// parse" — the same outcome `LoftyAudioMetadataReader` produces for those
/// cases. Also counts calls, so a test can assert the reader was never
/// consulted at all (e.g. for a non-audio file) — `metadata_for` staying
/// `None` alone can't distinguish "never called" from "called, but its
/// result was rejected/discarded downstream."
#[derive(Debug, Default, Clone)]
pub struct FakeAudioMetadataReader {
    tags: Arc<Mutex<HashMap<String, AudioTags>>>,
    call_count: Arc<Mutex<usize>>,
}

impl FakeAudioMetadataReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the tags `read()` returns for `path`.
    pub fn seed(&self, path: &str, tags: AudioTags) -> &Self {
        self.tags.lock().unwrap().insert(path.to_string(), tags);
        self
    }

    /// How many times `read()` has been called.
    pub fn call_count(&self) -> usize {
        *self.call_count.lock().unwrap()
    }
}

impl AudioMetadataReader for FakeAudioMetadataReader {
    async fn read(&self, path: &str) -> Option<AudioTags> {
        *self.call_count.lock().unwrap() += 1;
        self.tags.lock().unwrap().get(path).cloned()
    }
}

/// In-memory image-EXIF reader (issue #44 image slice). `read()` answers
/// `None` for any path with no seeded tags, mirroring "no EXIF found /
/// couldn't parse" — the same outcome `ExifImageMetadataReader` produces
/// for those cases. Also counts calls, so a test can assert the reader was
/// never consulted at all (e.g. for a non-image file).
#[derive(Debug, Default, Clone)]
pub struct FakeImageMetadataReader {
    tags: Arc<Mutex<HashMap<String, ImageTags>>>,
    call_count: Arc<Mutex<usize>>,
}

impl FakeImageMetadataReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the tags `read()` returns for `path`.
    pub fn seed(&self, path: &str, tags: ImageTags) -> &Self {
        self.tags.lock().unwrap().insert(path.to_string(), tags);
        self
    }

    /// How many times `read()` has been called.
    pub fn call_count(&self) -> usize {
        *self.call_count.lock().unwrap()
    }
}

impl ImageMetadataReader for FakeImageMetadataReader {
    async fn read(&self, path: &str) -> Option<ImageTags> {
        *self.call_count.lock().unwrap() += 1;
        self.tags.lock().unwrap().get(path).cloned()
    }
}

/// In-memory document reader (issue #44 document slice). `read()` answers
/// `None` for any path with no seeded tags, mirroring "unsupported
/// extension / no metadata / couldn't parse" — the same outcome
/// `PdfEpubMetadataReader` produces for those cases. Also counts calls, so
/// a test can assert the reader was never consulted at all (e.g. for a
/// non-document file).
#[derive(Debug, Default, Clone)]
pub struct FakeDocumentMetadataReader {
    tags: Arc<Mutex<HashMap<String, DocumentTags>>>,
    call_count: Arc<Mutex<usize>>,
}

impl FakeDocumentMetadataReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the tags `read()` returns for `path`.
    pub fn seed(&self, path: &str, tags: DocumentTags) -> &Self {
        self.tags.lock().unwrap().insert(path.to_string(), tags);
        self
    }

    /// How many times `read()` has been called.
    pub fn call_count(&self) -> usize {
        *self.call_count.lock().unwrap()
    }
}

impl DocumentMetadataReader for FakeDocumentMetadataReader {
    async fn read(&self, path: &str) -> Option<DocumentTags> {
        *self.call_count.lock().unwrap() += 1;
        self.tags.lock().unwrap().get(path).cloned()
    }
}

/// In-memory video reader (issue #44 video slice). `read()` answers
/// `None` for any path with no seeded tags, mirroring "couldn't open
/// container / no video stream / no metadata" — the same outcome
/// `FfmpegVideoMetadataReader` produces for those cases. Also counts
/// calls, so a test can assert the reader was never consulted at all
/// (e.g. for a non-video file).
#[derive(Debug, Default, Clone)]
pub struct FakeVideoMetadataReader {
    tags: Arc<Mutex<HashMap<String, VideoTags>>>,
    call_count: Arc<Mutex<usize>>,
}

impl FakeVideoMetadataReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the tags `read()` returns for `path`.
    pub fn seed(&self, path: &str, tags: VideoTags) -> &Self {
        self.tags.lock().unwrap().insert(path.to_string(), tags);
        self
    }

    /// How many times `read()` has been called.
    pub fn call_count(&self) -> usize {
        *self.call_count.lock().unwrap()
    }
}

impl VideoMetadataReader for FakeVideoMetadataReader {
    async fn read(&self, path: &str) -> Option<VideoTags> {
        *self.call_count.lock().unwrap() += 1;
        self.tags.lock().unwrap().get(path).cloned()
    }
}

/// In-memory comic reader (issue #44 comic slice). `read()` answers
/// `None` for any path with no seeded tags, mirroring "couldn't open
/// archive / unsupported extension" — the same outcome
/// `CbzComicMetadataReader` produces for those cases. Also counts calls,
/// so a test can assert the reader was never consulted at all (e.g. for
/// a non-comic file).
#[derive(Debug, Default, Clone)]
pub struct FakeComicMetadataReader {
    tags: Arc<Mutex<HashMap<String, ComicTags>>>,
    call_count: Arc<Mutex<usize>>,
}

impl FakeComicMetadataReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the tags `read()` returns for `path`.
    pub fn seed(&self, path: &str, tags: ComicTags) -> &Self {
        self.tags.lock().unwrap().insert(path.to_string(), tags);
        self
    }

    /// How many times `read()` has been called.
    pub fn call_count(&self) -> usize {
        *self.call_count.lock().unwrap()
    }
}

impl ComicMetadataReader for FakeComicMetadataReader {
    async fn read(&self, path: &str) -> Option<ComicTags> {
        *self.call_count.lock().unwrap() += 1;
        self.tags.lock().unwrap().get(path).cloned()
    }
}

/// In-memory `CatalogRunRepository` (UC-42). Lets the index/refresh handler
/// tests assert the run lifecycle without a database.
#[derive(Debug, Default, Clone)]
pub struct FakeCatalogRunRepository {
    runs: Arc<Mutex<HashMap<Uuid, CatalogRun>>>,
}

impl FakeCatalogRunRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// The recorded run for `id`, for assertions.
    pub fn get_recorded(&self, id: Uuid) -> Option<CatalogRun> {
        self.runs.lock().unwrap().get(&id).cloned()
    }

    pub fn count(&self) -> usize {
        self.runs.lock().unwrap().len()
    }
}

impl CatalogRunRepository for FakeCatalogRunRepository {
    async fn start(
        &self,
        id: Uuid,
        kind: RunKind,
        root: Option<&str>,
        started_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        self.runs.lock().unwrap().insert(
            id,
            CatalogRun {
                id,
                kind,
                status: RunStatus::Running,
                root: root.map(str::to_string),
                started_at,
                finished_at: None,
                counts: None,
                error: None,
            },
        );
        Ok(())
    }

    async fn finish(
        &self,
        id: Uuid,
        counts: RunCounts,
        finished_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        let mut runs = self.runs.lock().unwrap();
        if let Some(run) = runs.get_mut(&id) {
            // Mirrors the SQLite adapter's guard: the counts variant must
            // match the run's own kind, or the write is rejected rather than
            // silently leaving the wrong tally in place.
            let matches = matches!(
                (run.kind, &counts),
                (RunKind::Index, RunCounts::Index { .. })
                    | (RunKind::Refresh, RunCounts::Refresh { .. })
            );
            if !matches {
                return Err(DomainError::internal(format!(
                    "counts kind mismatch: run is {:?} but counts are {:?}",
                    run.kind, counts
                )));
            }
            run.status = RunStatus::Complete;
            run.counts = Some(counts);
            run.finished_at = Some(finished_at);
        }
        Ok(())
    }

    async fn fail(
        &self,
        id: Uuid,
        error: &str,
        finished_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        let mut runs = self.runs.lock().unwrap();
        if let Some(run) = runs.get_mut(&id) {
            run.status = RunStatus::Failed;
            run.error = Some(error.to_string());
            run.finished_at = Some(finished_at);
        }
        Ok(())
    }

    async fn get(&self, id: Uuid) -> Result<Option<CatalogRun>, DomainError> {
        Ok(self.runs.lock().unwrap().get(&id).cloned())
    }

    async fn interrupt_running(&self, now: DateTime<Utc>) -> Result<u64, DomainError> {
        let mut runs = self.runs.lock().unwrap();
        let mut reconciled = 0;
        for run in runs.values_mut() {
            if run.status == RunStatus::Running {
                run.status = RunStatus::Interrupted;
                run.finished_at = Some(now);
                reconciled += 1;
            }
        }
        Ok(reconciled)
    }
}

/// A `CatalogRepository` whose `list_all` always fails (UC-42 / FR-FC-27).
/// Drives `RefreshHandler::execute`'s "the walk could not proceed at all"
/// path, which is the only case that records a run `failed`. Every other
/// method is unreachable from that path, so it panics loudly if the walk
/// ever changes to call one of them.
#[derive(Debug, Default, Clone, Copy)]
pub struct FailingCatalogRepository;

impl CatalogRepository for FailingCatalogRepository {
    async fn find_by_path(&self, _path: &str) -> Result<Option<File>, DomainError> {
        unimplemented!("not reached by the run-fails-to-list path")
    }

    async fn find_by_uuid(&self, _uuid: Uuid) -> Result<Option<File>, DomainError> {
        unimplemented!("not reached by the run-fails-to-list path")
    }

    async fn insert_file(&self, _new_file: NewFile) -> Result<File, DomainError> {
        unimplemented!("not reached by the run-fails-to-list path")
    }

    async fn list_all(&self) -> Result<Vec<File>, DomainError> {
        Err(DomainError::Disk("catalog store unavailable".into()))
    }

    async fn refresh_hash(
        &self,
        _path: &str,
        _content_hash: &str,
        _indexed_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        unimplemented!("not reached by the run-fails-to-list path")
    }

    async fn mark_missing(
        &self,
        _path: &str,
        _missing_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        unimplemented!("not reached by the run-fails-to-list path")
    }

    async fn update_metadata(
        &self,
        _uuid: Uuid,
        _metadata: &SubtypeMetadata,
    ) -> Result<(), DomainError> {
        unimplemented!("not reached by the run-fails-to-list path")
    }

    async fn list_filtered(
        &self,
        _file_type: Option<FileType>,
        _state: StateFilter,
        _collection_uuid: Option<Uuid>,
    ) -> Result<Vec<File>, DomainError> {
        unimplemented!("not reached by the run-fails-to-list path")
    }

    async fn find_metadata_by_uuid(
        &self,
        _uuid: Uuid,
    ) -> Result<Option<SubtypeMetadata>, DomainError> {
        unimplemented!("not reached by the run-fails-to-list path")
    }

    async fn set_image_dimensions(
        &self,
        _uuid: Uuid,
        _width: i64,
        _height: i64,
    ) -> Result<(), DomainError> {
        unimplemented!("not reached by the run-fails-to-list path")
    }

    async fn find_image_dimensions(&self, _uuid: Uuid) -> Result<Option<(i64, i64)>, DomainError> {
        unimplemented!("not reached by the run-fails-to-list path")
    }

    async fn set_document_page_count(
        &self,
        _uuid: Uuid,
        _page_count: i64,
    ) -> Result<(), DomainError> {
        unimplemented!("not reached by the run-fails-to-list path")
    }

    async fn find_document_page_count(&self, _uuid: Uuid) -> Result<Option<i64>, DomainError> {
        unimplemented!("not reached by the run-fails-to-list path")
    }

    async fn set_video_duration(
        &self,
        _uuid: Uuid,
        _duration_seconds: f64,
    ) -> Result<(), DomainError> {
        unimplemented!("not reached by the run-fails-to-list path")
    }

    async fn find_video_duration(&self, _uuid: Uuid) -> Result<Option<f64>, DomainError> {
        unimplemented!("not reached by the run-fails-to-list path")
    }

    async fn set_comic_page_count(&self, _uuid: Uuid, _page_count: i64) -> Result<(), DomainError> {
        unimplemented!("not reached by the run-fails-to-list path")
    }

    async fn find_comic_page_count(&self, _uuid: Uuid) -> Result<Option<i64>, DomainError> {
        unimplemented!("not reached by the run-fails-to-list path")
    }

    async fn rename_file(
        &self,
        _uuid: Uuid,
        _new_name: &str,
        _new_path: &str,
    ) -> Result<File, DomainError> {
        unimplemented!("not reached by the run-fails-to-list path")
    }

    async fn soft_delete(
        &self,
        _uuid: Uuid,
        _deleted_at: DateTime<Utc>,
    ) -> Result<File, DomainError> {
        unimplemented!("not reached by the run-fails-to-list path")
    }

    async fn restore(&self, _uuid: Uuid) -> Result<File, DomainError> {
        unimplemented!("not reached by the run-fails-to-list path")
    }

    async fn purge(&self, _uuid: Uuid) -> Result<(), DomainError> {
        unimplemented!("not reached by the run-fails-to-list path")
    }

    async fn set_collection(&self, _uuid: Uuid, _collection_uuid: Uuid) -> Result<(), DomainError> {
        unimplemented!("not reached by the run-fails-to-list path")
    }

    async fn clear_collection(
        &self,
        _uuid: Uuid,
        _collection_uuid: Uuid,
    ) -> Result<(), DomainError> {
        unimplemented!("not reached by the run-fails-to-list path")
    }
}

/// A `Filesystem` whose `list_files` always fails, `path_exists` always
/// answers `true` (so `IndexHandler::start`'s root-exists guard passes and
/// the failure surfaces from `execute`'s walk instead). Drives the "the walk
/// could not proceed at all" path (UC-42 / FR-FC-27), the only case that
/// records an index run `failed`. Every other method is unreachable from
/// that path.
#[derive(Debug, Default, Clone, Copy)]
pub struct FailingListFilesystem;

impl Filesystem for FailingListFilesystem {
    async fn path_exists(&self, _root: &str) -> bool {
        true
    }

    async fn list_files(&self, _root: &str) -> Result<Vec<FileEntry>, DomainError> {
        Err(DomainError::Disk("filesystem unavailable".into()))
    }

    async fn content_hash(&self, _path: &str) -> Result<String, DomainError> {
        unimplemented!("not reached by the run-fails-to-list path")
    }

    async fn rename(&self, _from: &str, _to: &str) -> Result<(), DomainError> {
        unimplemented!("not reached by the run-fails-to-list path")
    }

    async fn remove_file(&self, _path: &str) -> Result<bool, DomainError> {
        unimplemented!("not reached by the run-fails-to-list path")
    }

    async fn read_file(&self, _path: &str) -> Result<String, DomainError> {
        unimplemented!("not reached by the run-fails-to-list path")
    }

    async fn write_file(&self, _path: &str, _content: &str) -> Result<(), DomainError> {
        unimplemented!("not reached by the run-fails-to-list path")
    }
}

/// A `CatalogRunRepository` whose `finish` always fails (UC-42 / FR-FC-27).
/// Pins the "a bookkeeping failure must not sink a successful walk" behavior:
/// `execute()` retries the write and, once retries are exhausted, still
/// returns the outcome it computed rather than propagating the recording
/// error. `start` and `fail` are not exercised by that test and are left
/// `unimplemented!()`.
#[derive(Debug, Default, Clone, Copy)]
pub struct FailingCatalogRunRepository;

impl CatalogRunRepository for FailingCatalogRunRepository {
    async fn start(
        &self,
        _id: Uuid,
        _kind: RunKind,
        _root: Option<&str>,
        _started_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        unimplemented!("not exercised by the finish-always-fails test")
    }

    async fn finish(
        &self,
        _id: Uuid,
        _counts: RunCounts,
        _finished_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        Err(DomainError::Disk("run store unavailable".into()))
    }

    async fn fail(
        &self,
        _id: Uuid,
        _error: &str,
        _finished_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        unimplemented!("not exercised by the finish-always-fails test")
    }

    async fn get(&self, _id: Uuid) -> Result<Option<CatalogRun>, DomainError> {
        unimplemented!("not exercised by the finish-always-fails test")
    }

    async fn interrupt_running(&self, _now: DateTime<Utc>) -> Result<u64, DomainError> {
        unimplemented!("not exercised by the finish-always-fails test")
    }
}
