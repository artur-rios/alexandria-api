use chrono::{DateTime, Utc};
use futures_util::stream::{self, StreamExt};
use serde::Serialize;
use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::audio_tags::AudioMetadataReader;
use crate::catalog::classify::classify_by_extension;
use crate::catalog::clock::Clock;
use crate::catalog::comic_tags::ComicMetadataReader;
use crate::catalog::document_tags::DocumentMetadataReader;
use crate::catalog::fs::{FileEntry, Filesystem};
use crate::catalog::image_tags::ImageMetadataReader;
use crate::catalog::model::{FileType, NewFile};
use crate::catalog::repos::CatalogRepository;
use crate::catalog::video_tags::VideoMetadataReader;
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
/// `start` authenticates the caller, validates the root path — it must exist,
/// and it must sit inside the configured `filesystem.root` when one is set
/// (FR-FC-26) — and returns a fresh run id immediately. The heavy `execute`
/// walk hashes and persists each supported file, skipping already-cataloged
/// paths. `start` and `execute` are separated so the HTTP/FFI layer can spawn
/// `execute` in the background (FR-FC-08) while `start` returns `202` right
/// away.
///
/// `execute` processes up to `concurrency` files at a time (configurable via
/// `indexing.concurrency`, default 4). The per-file work is dominated by
/// hashing the bytes, which `StdFilesystem` runs on Tokio's blocking pool, so
/// the concurrency buys real parallelism rather than interleaved waiting —
/// that is what NFR-02's throughput target rests on. It is bounded rather
/// than unlimited because an unbounded fan-out over a large library would
/// queue one blocking task per file and starve every other user of the
/// blocking pool. Note that the *database* half of each file's work still
/// serializes: SQLite admits one writer at a time, and the pool caps
/// connections at 8, so raising `concurrency` past that only lengthens the
/// queue in front of the writer.
///
/// Generic over its collaborators so the same decision logic is unit-tested
/// against trait fakes (no real DB, filesystem, or auth service in unit
/// tests), then wired with the concrete Sqlite/StdFilesystem/Bearer/services
/// at runtime.
pub struct IndexHandler<A, R, F, C, M, N, O, P, Q> {
    auth: A,
    repo: R,
    fs: F,
    clock: C,
    audio_tags: M,
    image_tags: N,
    document_tags: O,
    video_tags: P,
    comic_tags: Q,
    concurrency: usize,
    /// The configured library root (`filesystem.root`) every requested index
    /// root must sit inside (FR-FC-26). `None` when the key is unset, which
    /// leaves indexing unconstrained — the historical behaviour.
    library_root: Option<String>,
}

/// The client-facing rejection message for FR-FC-26 when the *requested*
/// root is genuinely outside the configured library root. Deliberately free
/// of the configured root's absolute path: the caller does not need to be
/// told where the library lives in order to learn that its request was out
/// of bounds.
const OUTSIDE_LIBRARY_ROOT: &str = "root path is outside the configured library root";

/// The client-facing rejection message for FR-FC-26 when the *server's*
/// `filesystem.root` configuration itself cannot be resolved. Deliberately
/// distinct from [`OUTSIDE_LIBRARY_ROOT`]: that message implies the caller's
/// request was wrong, which is misleading here — the caller's root may be
/// perfectly fine, and it is the server's configuration that needs fixing.
/// Still free of the configured root's absolute path — naming the failure
/// mode is not the same as naming the path.
const LIBRARY_ROOT_UNRESOLVABLE: &str =
    "the server's configured library root could not be resolved; contact the operator";

/// What one scanned entry resolved to. Returned by the per-entry future so
/// the concurrent walk can tally outcomes without sharing a counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryOutcome {
    Indexed,
    Skipped,
    Failed,
}

impl<A, R, F, C, M, N, O, P, Q> IndexHandler<A, R, F, C, M, N, O, P, Q>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
    C: Clock,
    M: AudioMetadataReader,
    N: ImageMetadataReader,
    O: DocumentMetadataReader,
    P: VideoMetadataReader,
    Q: ComicMetadataReader,
{
    /// `concurrency` is how many files `execute` processes at a time
    /// (`indexing.concurrency`). Zero is meaningless — a stream buffered zero
    /// deep makes no progress — so it is clamped to 1, which is the
    /// sequential behaviour a caller asking for "no concurrency" means.
    ///
    /// `library_root` is the configured `filesystem.root` (FR-FC-26). An
    /// empty string means the key is unset, and indexing stays unconstrained
    /// exactly as it was before the constraint existed — the constraint is
    /// opt-in by configuration, so no existing deployment changes behaviour
    /// on upgrade.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        auth: A,
        repo: R,
        fs: F,
        clock: C,
        audio_tags: M,
        image_tags: N,
        document_tags: O,
        video_tags: P,
        comic_tags: Q,
        concurrency: u32,
        library_root: String,
    ) -> Self {
        let library_root = {
            let trimmed = library_root.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        };
        Self {
            auth,
            repo,
            fs,
            clock,
            audio_tags,
            image_tags,
            document_tags,
            video_tags,
            comic_tags,
            concurrency: concurrency.max(1) as usize,
            library_root,
        }
    }

    /// FR-FC-26: the requested root must be the configured library root or a
    /// descendant of it. Returns `Ok(())` unconditionally when no library
    /// root is configured.
    ///
    /// Both sides are canonicalized before comparison. That is what makes the
    /// check hold against `<root>/../../etc` (the traversal is resolved away),
    /// against `<root>` vs `<root>/` vs `<root>/.` (all resolve to the same
    /// path), and against a symlinked root (both sides resolve to the link
    /// target). The comparison itself is `Path::starts_with`, which matches
    /// whole path components — a string prefix test would let `/library-evil`
    /// slip past a `/library` bound.
    fn check_root_within_library(&self, requested: &str) -> Result<(), DomainError> {
        let Some(library_root) = self.library_root.as_deref() else {
            return Ok(());
        };
        // A configured root that cannot be resolved is a misconfiguration, not
        // a caller error. Fail the request rather than silently degrading to
        // unconstrained indexing: a security bound that disappears when its
        // configuration is wrong is worse than no bound at all, because the
        // operator believes it is there. The process still starts and every
        // other operation still works — only indexing is refused, and the log
        // names the key to fix.
        let canonical_library_root = match std::fs::canonicalize(library_root) {
            Ok(path) => path,
            Err(err) => {
                tracing::error!(
                    root = %library_root,
                    error = %err,
                    "configured filesystem.root cannot be resolved; refusing to index until it is fixed"
                );
                return Err(DomainError::InvalidInput(LIBRARY_ROOT_UNRESOLVABLE.into()));
            }
        };
        // The requested root's existence was already checked above, so a
        // canonicalization failure here means the path cannot be resolved to
        // something comparable. Fail closed.
        let canonical_requested = std::fs::canonicalize(requested)
            .map_err(|_| DomainError::InvalidInput(OUTSIDE_LIBRARY_ROOT.into()))?;
        if canonical_requested.starts_with(&canonical_library_root) {
            Ok(())
        } else {
            Err(DomainError::InvalidInput(OUTSIDE_LIBRARY_ROOT.into()))
        }
    }

    /// Validate and start — returns a run id without doing any scanning.
    pub async fn start(
        &self,
        request: IndexRequest,
        token: &str,
    ) -> Result<IndexStarted, DomainError> {
        self.auth.authenticate(token).await?;
        if !self.fs.path_exists(&request.root).await {
            return Err(DomainError::InvalidInput("root path does not exist".into()));
        }
        self.check_root_within_library(&request.root)?;
        Ok(IndexStarted {
            run_id: Uuid::new_v4(),
        })
    }

    /// Walk, classify, hash, and persist. Skips unsupported extensions and
    /// paths already cataloged (AF-03). Completion is logged at `info`.
    ///
    /// Up to `concurrency` entries are in flight at once. The order files are
    /// processed in is therefore unspecified — the outcome counts are not,
    /// since each entry contributes exactly one outcome regardless of when it
    /// finishes. Two entries naming the same path would race the
    /// already-cataloged check (AF-03), but `list_files` cannot produce a path
    /// twice, and the `files.path` unique constraint turns any such duplicate
    /// into that entry's own `failed` rather than a corrupt second record.
    ///
    /// A failure that concerns one specific file — its bytes cannot be read, or
    /// a repository write for it fails — is counted in `failed`, logged at
    /// `warn`, and the walk continues. One locked file must not abandon the
    /// rest of the library. Only a failure to list the root at all aborts.
    pub async fn execute(&self, root: &str, run_id: Uuid) -> Result<IndexOutcome, DomainError> {
        let now = self.clock.now();
        let entries = self.fs.list_files(root).await?;
        let scanned = entries.len();

        let (indexed, skipped, failed) = stream::iter(entries)
            .map(|entry| async move {
                let Some(file_type) = classify_by_extension(&entry.name) else {
                    return EntryOutcome::Skipped;
                };
                let path = entry.path.clone();
                match self.index_entry(entry, file_type, now).await {
                    Ok(true) => EntryOutcome::Indexed,
                    Ok(false) => EntryOutcome::Skipped,
                    Err(err) => {
                        tracing::warn!(
                            %run_id,
                            path = %path,
                            error = %err,
                            "skipping file that could not be indexed"
                        );
                        EntryOutcome::Failed
                    }
                }
            })
            .buffer_unordered(self.concurrency)
            .fold((0usize, 0usize, 0usize), |counts, outcome| async move {
                let (indexed, skipped, failed) = counts;
                match outcome {
                    EntryOutcome::Indexed => (indexed + 1, skipped, failed),
                    EntryOutcome::Skipped => (indexed, skipped + 1, failed),
                    EntryOutcome::Failed => (indexed, skipped, failed + 1),
                }
            })
            .await;

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
        let file = self
            .repo
            .insert_file(NewFile {
                uuid: Uuid::new_v4(),
                path: entry.path.clone(),
                name: entry.name,
                file_type,
                content_hash,
                indexed_at: now,
            })
            .await?;

        // Best-effort audio tag prefill (issue #44 pilot). Extraction only
        // ever runs here, at first index — refresh never touches metadata.
        // A parse failure or a write failure here must not fail indexing
        // (it is not counted in `IndexOutcome::failed`).
        if file_type == FileType::Audio {
            if let Some(metadata) = self
                .audio_tags
                .read(&entry.path)
                .await
                .and_then(|tags| tags.into_subtype_metadata())
            {
                if let Err(err) = self.repo.update_metadata(file.uuid, &metadata).await {
                    tracing::warn!(
                        path = %entry.path,
                        error = %err,
                        "indexed but failed to write extracted audio tags"
                    );
                }
            }
        }

        // Best-effort image EXIF prefill (issue #44 image slice). Two
        // independent writes: dimensions (outside SubtypeMetadata, via
        // set_image_dimensions) and title (via the shared update_metadata,
        // same as audio). Neither write's failure blocks the other or fails
        // indexing.
        if file_type == FileType::Image {
            if let Some(tags) = self.image_tags.read(&entry.path).await {
                if let (Some(width), Some(height)) = (tags.width, tags.height) {
                    if let Err(err) = self
                        .repo
                        .set_image_dimensions(file.uuid, width, height)
                        .await
                    {
                        tracing::warn!(
                            path = %entry.path,
                            error = %err,
                            "indexed but failed to write extracted image dimensions"
                        );
                    }
                }
                if let Some(title) = tags.title {
                    // caption: None is safe only because extraction runs
                    // exactly once, at first index, before an owner could
                    // have set one via UC-04 — update_metadata is a full
                    // replace, so reusing this pattern anywhere caption
                    // might already be set would silently wipe it.
                    let metadata = crate::catalog::model::SubtypeMetadata::Image {
                        title: Some(title),
                        caption: None,
                    };
                    if let Err(err) = self.repo.update_metadata(file.uuid, &metadata).await {
                        tracing::warn!(
                            path = %entry.path,
                            error = %err,
                            "indexed but failed to write extracted image title"
                        );
                    }
                }
            }
        }

        // Best-effort document metadata prefill (issue #44 document
        // slice). Two independent writes: page count (outside
        // SubtypeMetadata, via set_document_page_count — PDF only, EPUB
        // never sets it) and title/author/year/format_kind (via the
        // shared update_metadata). Neither write's failure blocks the
        // other or fails indexing.
        if file_type == FileType::Document {
            if let Some(tags) = self.document_tags.read(&entry.path).await {
                if let Some(page_count) = tags.page_count {
                    if let Err(err) = self
                        .repo
                        .set_document_page_count(file.uuid, page_count)
                        .await
                    {
                        tracing::warn!(
                            path = %entry.path,
                            error = %err,
                            "indexed but failed to write extracted document page count"
                        );
                    }
                }
                if tags.title.is_some()
                    || tags.author.is_some()
                    || tags.year.is_some()
                    || tags.format_kind.is_some()
                {
                    let metadata = crate::catalog::model::SubtypeMetadata::Document {
                        title: tags.title,
                        author: tags.author,
                        year: tags.year,
                        format_kind: tags.format_kind,
                    };
                    if let Err(err) = self.repo.update_metadata(file.uuid, &metadata).await {
                        tracing::warn!(
                            path = %entry.path,
                            error = %err,
                            "indexed but failed to write extracted document metadata"
                        );
                    }
                }
            }
        }

        // Best-effort video metadata prefill (issue #44 video slice). Two
        // independent writes: duration (outside SubtypeMetadata, via
        // set_video_duration) and title/year/resolution (via the shared
        // update_metadata, media_kind always None — it is not inferable
        // from the file). Neither write's failure blocks the other or
        // fails indexing.
        if file_type == FileType::Video {
            if let Some(tags) = self.video_tags.read(&entry.path).await {
                if let Some(crate::catalog::video_tags::VideoDuration(duration_seconds)) =
                    tags.duration_seconds
                {
                    if let Err(err) = self
                        .repo
                        .set_video_duration(file.uuid, duration_seconds)
                        .await
                    {
                        tracing::warn!(
                            path = %entry.path,
                            error = %err,
                            "indexed but failed to write extracted video duration"
                        );
                    }
                }
                if tags.title.is_some() || tags.year.is_some() || tags.resolution.is_some() {
                    let metadata = crate::catalog::model::SubtypeMetadata::Video {
                        title: tags.title,
                        year: tags.year,
                        resolution: tags.resolution,
                        media_kind: None,
                    };
                    if let Err(err) = self.repo.update_metadata(file.uuid, &metadata).await {
                        tracing::warn!(
                            path = %entry.path,
                            error = %err,
                            "indexed but failed to write extracted video metadata"
                        );
                    }
                }
            }
        }

        // Best-effort comic metadata prefill (issue #44 comic slice). Two
        // independent writes: page count (outside SubtypeMetadata, via
        // set_comic_page_count — always present once the archive opens)
        // and title/series/issue_number (via the shared update_metadata).
        // Neither write's failure blocks the other or fails indexing.
        if file_type == FileType::Comic {
            if let Some(tags) = self.comic_tags.read(&entry.path).await {
                if let Some(page_count) = tags.page_count {
                    if let Err(err) = self.repo.set_comic_page_count(file.uuid, page_count).await {
                        tracing::warn!(
                            path = %entry.path,
                            error = %err,
                            "indexed but failed to write extracted comic page count"
                        );
                    }
                }
                if tags.title.is_some() || tags.series.is_some() || tags.issue_number.is_some() {
                    let metadata = crate::catalog::model::SubtypeMetadata::Comic {
                        title: tags.title,
                        series: tags.series,
                        issue_number: tags.issue_number,
                    };
                    if let Err(err) = self.repo.update_metadata(file.uuid, &metadata).await {
                        tracing::warn!(
                            path = %entry.path,
                            error = %err,
                            "indexed but failed to write extracted comic metadata"
                        );
                    }
                }
            }
        }
        Ok(true)
    }
}
