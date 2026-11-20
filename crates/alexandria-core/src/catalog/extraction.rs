//! Reading a file's own metadata into the catalog (UC-01, UC-02).
//!
//! Extraction used to live inside `IndexHandler::index_entry`, and only
//! there — the comment above it said so: *"Extraction only ever runs here, at
//! first index — refresh never touches metadata."* That is what left a
//! library indexed by an older build carrying gaps nothing could close. The
//! artists list is the case that surfaced it: `album_artist` arrived in
//! migration 15, every row written before it holds NULL, and a list grouped
//! by the album's artist fell back to each track's performer for all of
//! them — so a record with guests on it showed up once per guest.
//!
//! So the reading lives here now, in one place both commands use: an index
//! writes what it reads into a fresh row, and a refresh fills what an older
//! extraction left empty. The difference between "write" and "fill" is the
//! repository's — [`CatalogRepository::update_metadata`] replaces, while
//! [`CatalogRepository::fill_missing_metadata`] only ever adds — and it is
//! the difference that makes revisiting a row safe: an owner's own
//! corrections (UC-04) are not overwritten by whatever the tags say.

use uuid::Uuid;

use crate::catalog::audio_tags::{AudioDuration, AudioMetadataReader};
use crate::catalog::comic_tags::ComicMetadataReader;
use crate::catalog::document_tags::DocumentMetadataReader;
use crate::catalog::image_tags::ImageMetadataReader;
use crate::catalog::model::{FileType, SubtypeMetadata};
use crate::catalog::repos::CatalogRepository;
use crate::catalog::video_tags::{VideoDuration, VideoMetadataReader};

/// How an extraction's editable fields reach the catalog.
///
/// The two commands want opposite things from the same reading, and getting
/// that backwards would be expensive in a way no test of the reading itself
/// would catch — see the variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataWrite {
    /// Write what was read, replacing whatever the row held.
    ///
    /// A fresh row, at first index: there is nothing to overwrite, and the
    /// fields tags do not carry are genuinely empty.
    Replace,
    /// Fill only the columns the catalog is missing.
    ///
    /// A row that already exists, at refresh. A replace here would put a
    /// file's tags over a title the owner had corrected by hand, and would
    /// blank an image's caption or a video's media kind — fields no tag
    /// carries, which a full replace writes as NULL.
    FillGaps,
}

/// The five readers, held together so a command takes one collaborator
/// rather than five.
///
/// `IndexHandler` still names all five in its own constructor — its callers
/// and its tests were written that way — and builds this from them.
pub struct MetadataExtractor<M, N, O, P, Q> {
    audio: M,
    image: N,
    document: O,
    video: P,
    comic: Q,
}

impl<M, N, O, P, Q> MetadataExtractor<M, N, O, P, Q>
where
    M: AudioMetadataReader,
    N: ImageMetadataReader,
    O: DocumentMetadataReader,
    P: VideoMetadataReader,
    Q: ComicMetadataReader,
{
    /// Holds the five readers.
    pub fn new(audio: M, image: N, document: O, video: P, comic: Q) -> Self {
        Self {
            audio,
            image,
            document,
            video,
            comic,
        }
    }

    /// Reads `path`'s own metadata into the catalog row `uuid` names.
    ///
    /// Best-effort throughout, and that is deliberate: a file whose tags
    /// cannot be parsed, or a write that fails, is logged and stepped over.
    /// Neither fails the run that called this — an unreadable tag is not a
    /// file that could not be catalogued.
    ///
    /// Answers whether anything was read at all, which is what tells the
    /// caller a row is worth stamping: a file that gave up nothing is left
    /// behind the current version so a later run can try it again.
    pub async fn extract_into<R: CatalogRepository>(
        &self,
        repo: &R,
        uuid: Uuid,
        path: &str,
        file_type: FileType,
        write: MetadataWrite,
    ) -> bool {
        match file_type {
            FileType::Audio => self.audio_into(repo, uuid, path, write).await,
            FileType::Image => self.image_into(repo, uuid, path, write).await,
            FileType::Document => self.document_into(repo, uuid, path, write).await,
            FileType::Video => self.video_into(repo, uuid, path, write).await,
            FileType::Comic => self.comic_into(repo, uuid, path, write).await,
            // Nothing to read: a text or HTML file's own bytes are its
            // content, and the catalog holds no subtype metadata for either.
            FileType::Text | FileType::Html => false,
        }
    }

    /// Writes `metadata` the way `write` says, logging and swallowing a
    /// failure — one file's write must not end a run over the whole library.
    async fn write_metadata<R: CatalogRepository>(
        &self,
        repo: &R,
        uuid: Uuid,
        path: &str,
        metadata: &SubtypeMetadata,
        write: MetadataWrite,
    ) {
        let result = match write {
            MetadataWrite::Replace => repo.update_metadata(uuid, metadata).await,
            MetadataWrite::FillGaps => repo.fill_missing_metadata(uuid, metadata).await,
        };

        if let Err(err) = result {
            tracing::warn!(
                path = %path,
                error = %err,
                "read the file's metadata but failed to write it"
            );
        }
    }

    async fn audio_into<R: CatalogRepository>(
        &self,
        repo: &R,
        uuid: Uuid,
        path: &str,
        write: MetadataWrite,
    ) -> bool {
        let Some(tags) = self.audio.read(path).await else {
            return false;
        };

        // Two independent writes: the duration, which sits outside
        // `SubtypeMetadata` because it is not owner-editable, and the tags
        // themselves. Neither failure blocks the other.
        if let Some(AudioDuration(duration_seconds)) = tags.duration_seconds {
            if let Err(err) = repo.set_audio_duration(uuid, duration_seconds).await {
                tracing::warn!(
                    path = %path,
                    error = %err,
                    "read the audio duration but failed to write it"
                );
            }
        }

        if let Some(metadata) = tags.into_subtype_metadata() {
            self.write_metadata(repo, uuid, path, &metadata, write)
                .await;
        }

        true
    }

    async fn image_into<R: CatalogRepository>(
        &self,
        repo: &R,
        uuid: Uuid,
        path: &str,
        write: MetadataWrite,
    ) -> bool {
        let Some(tags) = self.image.read(path).await else {
            return false;
        };

        if let (Some(width), Some(height)) = (tags.width, tags.height) {
            if let Err(err) = repo.set_image_dimensions(uuid, width, height).await {
                tracing::warn!(
                    path = %path,
                    error = %err,
                    "read the image dimensions but failed to write them"
                );
            }
        }

        if let Some(title) = tags.title {
            // `caption: None` is safe under either write: a replace only ever
            // happens on a fresh row, where there is no caption to lose, and
            // a fill leaves a caption the owner wrote exactly where it is.
            let metadata = SubtypeMetadata::Image {
                title: Some(title),
                caption: None,
            };
            self.write_metadata(repo, uuid, path, &metadata, write)
                .await;
        }

        true
    }

    async fn document_into<R: CatalogRepository>(
        &self,
        repo: &R,
        uuid: Uuid,
        path: &str,
        write: MetadataWrite,
    ) -> bool {
        let Some(tags) = self.document.read(path).await else {
            return false;
        };

        if let Some(page_count) = tags.page_count {
            if let Err(err) = repo.set_document_page_count(uuid, page_count).await {
                tracing::warn!(
                    path = %path,
                    error = %err,
                    "read the document page count but failed to write it"
                );
            }
        }

        if tags.title.is_some()
            || tags.author.is_some()
            || tags.year.is_some()
            || tags.format_kind.is_some()
        {
            let metadata = SubtypeMetadata::Document {
                title: tags.title,
                author: tags.author,
                year: tags.year,
                format_kind: tags.format_kind,
            };
            self.write_metadata(repo, uuid, path, &metadata, write)
                .await;
        }

        true
    }

    async fn video_into<R: CatalogRepository>(
        &self,
        repo: &R,
        uuid: Uuid,
        path: &str,
        write: MetadataWrite,
    ) -> bool {
        let Some(tags) = self.video.read(path).await else {
            return false;
        };

        if let Some(VideoDuration(duration_seconds)) = tags.duration_seconds {
            if let Err(err) = repo.set_video_duration(uuid, duration_seconds).await {
                tracing::warn!(
                    path = %path,
                    error = %err,
                    "read the video duration but failed to write it"
                );
            }
        }

        if tags.title.is_some() || tags.year.is_some() || tags.resolution.is_some() {
            // `media_kind: None` for the same reason the image's caption is:
            // no file carries it, so it is the owner's answer or nobody's.
            let metadata = SubtypeMetadata::Video {
                title: tags.title,
                year: tags.year,
                resolution: tags.resolution,
                media_kind: None,
            };
            self.write_metadata(repo, uuid, path, &metadata, write)
                .await;
        }

        true
    }

    async fn comic_into<R: CatalogRepository>(
        &self,
        repo: &R,
        uuid: Uuid,
        path: &str,
        write: MetadataWrite,
    ) -> bool {
        let Some(tags) = self.comic.read(path).await else {
            return false;
        };

        if let Some(page_count) = tags.page_count {
            if let Err(err) = repo.set_comic_page_count(uuid, page_count).await {
                tracing::warn!(
                    path = %path,
                    error = %err,
                    "read the comic page count but failed to write it"
                );
            }
        }

        if tags.title.is_some() || tags.series.is_some() || tags.issue_number.is_some() {
            let metadata = SubtypeMetadata::Comic {
                title: tags.title,
                series: tags.series,
                issue_number: tags.issue_number,
            };
            self.write_metadata(repo, uuid, path, &metadata, write)
                .await;
        }

        true
    }
}
