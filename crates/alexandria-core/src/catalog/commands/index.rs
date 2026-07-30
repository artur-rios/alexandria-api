use serde::Serialize;
use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::classify::classify_by_extension;
use crate::catalog::clock::Clock;
use crate::catalog::fs::Filesystem;
use crate::catalog::model::{NewFile};
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
    pub async fn execute(&self, root: &str, run_id: Uuid) -> Result<IndexOutcome, DomainError> {
        let now = self.clock.now();
        let entries = self.fs.list_files(root).await?;
        let scanned = entries.len();
        let mut indexed = 0usize;
        let mut skipped = 0usize;

        for entry in entries {
            let file_type = match classify_by_extension(&entry.name) {
                Some(t) => t,
                None => {
                    skipped += 1;
                    continue;
                }
            };
            if self.repo.find_by_path(&entry.path).await?.is_some() {
                skipped += 1;
                continue;
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
            indexed += 1;
        }

        tracing::info!(%run_id, scanned, indexed, skipped, "indexing complete");
        Ok(IndexOutcome {
            run_id,
            scanned,
            indexed,
            skipped,
        })
    }
}