use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::classify::classify_by_extension;
use crate::catalog::clock::Clock;
use crate::catalog::fs::{FileEntry, Filesystem};
use crate::catalog::model::{FileType, NewFile};
use crate::catalog::repos::CatalogRepository;
use crate::errors::DomainError;

#[derive(Debug, Clone)]
pub struct IndexRequest {
    pub root: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexStarted {
    #[serde(rename = "runId")]
    pub run_id: Uuid,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexOutcome {
    #[serde(rename = "runId")]
    pub run_id: Uuid,
    pub scanned: usize,
    pub indexed: usize,
    pub skipped: usize,
    /// Entries that could not be indexed because an operation against that one
    /// file failed (unreadable bytes, or a repository write error). The run
    /// continues past them; each is logged at `warn`.
    pub failed: usize,
}

/// Index library files (UC-01).
///
/// `start` authenticates the caller, validates the root path, and returns a
/// fresh run id immediately. The heavy `execute` walk hashes and persists each
/// supported file, skipping already-cataloged paths. `start` and `execute` are
/// separated so the HTTP/FFI layer can spawn `execute` in the background
/// (FR-FC-08) while `start` returns `202` right away.
///
/// Generic over its collaborators so the same decision logic is unit-tested
/// against trait fakes (no real DB, filesystem, or auth service in unit
/// tests), then wired with the concrete Sqlite/StdFilesystem/Bearer/services
/// at runtime.
pub struct IndexHandler<A, R, F, C> {
    auth: A,
    repo: R,
    fs: F,
    clock: C,
}

impl<A, R, F, C> IndexHandler<A, R, F, C>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
    C: Clock,
{
    pub fn new(auth: A, repo: R, fs: F, clock: C) -> Self {
        Self {
            auth,
            repo,
            fs,
            clock,
        }
    }

    /// Validate and start — returns a run id without doing any scanning.
    pub async fn start(&self, request: IndexRequest, token: &str) -> Result<IndexStarted, DomainError> {
        self.auth.authenticate(token).await?;
        if !self.fs.path_exists(&request.root).await {
            return Err(DomainError::InvalidInput("root path does not exist".into()));
        }
        Ok(IndexStarted {
            run_id: Uuid::new_v4(),
        })
    }

    /// Walk, classify, hash, and persist. Skips unsupported extensions and
    /// paths already cataloged (AF-03). Completion is logged at `info`.
    ///
    /// A failure that concerns one specific file — its bytes cannot be read, or
    /// a repository write for it fails — is counted in `failed`, logged at
    /// `warn`, and the walk continues. One locked file must not abandon the
    /// rest of the library. Only a failure to list the root at all aborts.
    pub async fn execute(&self, root: &str, run_id: Uuid) -> Result<IndexOutcome, DomainError> {
        let now = self.clock.now();
        let entries = self.fs.list_files(root).await?;
        let scanned = entries.len();
        let mut indexed = 0usize;
        let mut skipped = 0usize;
        let mut failed = 0usize;

        for entry in entries {
            let file_type = match classify_by_extension(&entry.name) {
                Some(t) => t,
                None => {
                    skipped += 1;
                    continue;
                }
            };
            let path = entry.path.clone();
            match self.index_entry(entry, file_type, now).await {
                Ok(true) => indexed += 1,
                Ok(false) => skipped += 1,
                Err(err) => {
                    failed += 1;
                    tracing::warn!(
                        %run_id,
                        path = %path,
                        error = %err,
                        "skipping file that could not be indexed"
                    );
                }
            }
        }

        tracing::info!(%run_id, scanned, indexed, skipped, failed, "indexing complete");
        Ok(IndexOutcome {
            run_id,
            scanned,
            indexed,
            skipped,
            failed,
        })
    }

    /// Index one already-classified entry. `Ok(true)` means a record was
    /// created, `Ok(false)` that the path was already cataloged (AF-03), and
    /// `Err` that this one file failed — the caller counts it and moves on.
    async fn index_entry(
        &self,
        entry: FileEntry,
        file_type: FileType,
        now: DateTime<Utc>,
    ) -> Result<bool, DomainError> {
        if self.repo.find_by_path(&entry.path).await?.is_some() {
            return Ok(false);
        }
        let content_hash = self.fs.content_hash(&entry.path).await?;
        self.repo
            .insert_file(NewFile {
                uuid: Uuid::new_v4(),
                path: entry.path,
                name: entry.name,
                file_type,
                content_hash,
                indexed_at: now,
            })
            .await?;
        Ok(true)
    }
}