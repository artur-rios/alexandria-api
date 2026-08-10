use chrono::{DateTime, Utc};
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
    ) -> Self {
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
