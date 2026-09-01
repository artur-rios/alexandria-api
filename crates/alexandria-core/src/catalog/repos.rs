use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::sqlite::SqlitePool;
use uuid::Uuid;

use crate::catalog::model::{
    File, FileState, FileType, FileView, FormatKind, LibraryScope, MediaKind, NewFile, StateFilter,
    SubtypeMetadata,
};
use crate::errors::{DomainError, WRITE_TX};

/// The largest number of `?` placeholders one batched `WHERE file_id IN
/// (…)` query binds (issue #116 §2). SQLite's compiled-in bound-parameter
/// ceiling (`SQLITE_MAX_VARIABLE_NUMBER`) is 999 on the conservative
/// builds still common in the wild and 32766 on builds compiled with
/// SQLite's newer default; this crate does not control how the SQLite this
/// binary links against was compiled, so `list_filtered_view` assumes the
/// lower, older limit rather than the host's actual one. A batch is chunked
/// at this size regardless of how many ids a listing produces, so the
/// query count for one subtype stays a fixed handful of chunks rather than
/// growing with the number of files.
const MAX_SQLITE_PARAMS: usize = 900;

/// Catalog repository port. The indexer depends on this trait so its decision
/// logic (skip duplicates, insert) is unit-tested against an in-memory fake
/// with no database (Testing Specification §6.2). The Sqlite implementation
/// persists File records and their subtype rows.
#[allow(async_fn_in_trait)]
pub trait CatalogRepository: Send + Sync {
    async fn find_by_path(&self, path: &str) -> Result<Option<File>, DomainError>;
    /// Look up a file by its public UUID (UC-04 AF-02 relies on this).
    async fn find_by_uuid(&self, uuid: Uuid) -> Result<Option<File>, DomainError>;
    async fn insert_file(&self, new_file: NewFile) -> Result<File, DomainError>;
    /// Every cataloged record (UC-02 re-index iterates these).
    async fn list_all(&self) -> Result<Vec<File>, DomainError>;
    /// Refresh a file's content hash, size, and mtime + `indexed_at`, and
    /// clear the missing marker. **UC-33's writer only**
    /// (`EditTextFileContentHandler`) — a text edit changes the file's bytes,
    /// size, *and* mtime all at once. Recording only the hash and not the new
    /// size/mtime would leave the row's stats stale, so the very next
    /// re-index would see a stat mismatch, treat the file as changed, and
    /// null out the hash this call just verified and stored (see
    /// `refresh_stat` below, which is what re-index calls). `state` /
    /// `deleted_at` untouched.
    async fn refresh_hash(
        &self,
        path: &str,
        content_hash: &str,
        size_bytes: i64,
        mtime: Option<DateTime<Utc>>,
        indexed_at: DateTime<Utc>,
    ) -> Result<(), DomainError>;
    /// Refresh a file's size and mtime + `indexed_at`, clear its (now stale)
    /// `content_hash`, and clear the missing marker. **UC-02's writer only**
    /// (`RefreshHandler`) — re-index compares stat, not bytes (Task 4 /
    /// FR-FC-10), so it never has a fresh hash to record; the recorded one
    /// described bytes that changed, so it is nulled rather than left to be
    /// served as current. `state` / `deleted_at` untouched.
    async fn refresh_stat(
        &self,
        path: &str,
        size_bytes: i64,
        mtime: Option<DateTime<Utc>>,
        indexed_at: DateTime<Utc>,
    ) -> Result<(), DomainError>;
    /// Mark a cataloged path's disk file as gone (UC-02 AF-01). Sets
    /// `missing_at`; leaves `state` (soft-delete is UC-06) and `deleted_at`.
    async fn mark_missing(&self, path: &str, missing_at: DateTime<Utc>) -> Result<(), DomainError>;
    /// Replace the editable subtype columns of the file identified by `uuid`
    /// (UC-04 / FR-FC-14..18). Full replace: every editable column listed in
    /// `SubtypeMetadata` is written, `None` writes `NULL`. Non-editable
    /// columns (`episodeCount`, `pageCount`, `width`, `height`, `sourceUrl`,
    /// `savedAt`) are left untouched. Returns `NotFound` when no file row
    /// carries the UUID and `InvalidInput` when the metadata variant does not
    /// match the file's `type` (the handler validates the latter first, but
    /// the repository defends the invariant too).
    async fn update_metadata(
        &self,
        uuid: Uuid,
        metadata: &SubtypeMetadata,
    ) -> Result<(), DomainError>;

    /// Fill the editable subtype columns the catalog is *missing* for `uuid`
    /// from `metadata`, and leave every column that already holds something
    /// exactly as it is (UC-02).
    ///
    /// The counterpart of [`CatalogRepository::update_metadata`], and the
    /// difference between them is the whole reason this exists: that one is a
    /// full replace, written for an owner editing a record on purpose. A
    /// re-extraction that used it would overwrite a title the owner had
    /// corrected with whatever the file's tags say, and would blank the
    /// fields tags cannot carry — an image's caption, a video's media kind —
    /// because a full replace writes `None` as `NULL`.
    ///
    /// So this one fills gaps and nothing else. A row indexed before the
    /// extractor could read a field gets that field; everything the owner or
    /// an earlier extraction wrote survives untouched. `NotFound` and the
    /// subtype mismatch behave as they do above.
    async fn fill_missing_metadata(
        &self,
        uuid: Uuid,
        metadata: &SubtypeMetadata,
    ) -> Result<(), DomainError>;

    /// Record which extraction wrote this file's metadata
    /// (`catalog::model::METADATA_VERSION`).
    ///
    /// Written after extraction has actually run, never at insert: a row
    /// whose tags could not be read must stay behind the current version so
    /// a later refresh tries again.
    async fn set_metadata_version(&self, uuid: Uuid, version: i64) -> Result<(), DomainError>;

    /// List files filtered by type, lifecycle state, and containing
    /// collection (UC-03 / FR-FC-12; the collection filter arrived with
    /// UC-14). `file_type = None` means no type filter; `state` selects the
    /// lifecycle subset (`Active` excludes `deleted`, `Deleted` only deleted,
    /// `All` both); `collection_uuid = None` means no collection filter, and
    /// a uuid that resolves to no collection matches no files. Ordered by
    /// path. Uses `idx_files_type` / `idx_files_state`.
    async fn list_filtered(
        &self,
        file_type: Option<FileType>,
        state: StateFilter,
        collection_uuid: Option<Uuid>,
        scope: LibraryScope,
    ) -> Result<Vec<File>, DomainError>;

    /// List files filtered exactly as `list_filtered`, but answering the
    /// full `FileView` record each row would carry from `get_by_uuid` —
    /// the file, its subtype metadata, and the extracted scalars (issue
    /// #116 / FR-FC-12).
    ///
    /// The obvious implementation calls the single-file assembly once per
    /// row, moving the client's old N+1 into the core rather than removing
    /// it. This does not: it runs the filtered query once, then issues one
    /// further query per subtype table the result actually contains (never
    /// per file), each pulling every matching row at once via `WHERE
    /// file_id IN (…)` — chunked at `MAX_SQLITE_PARAMS` so a library larger
    /// than SQLite's bound-parameter ceiling still succeeds instead of
    /// failing the query — and stitches the rows back onto their files in
    /// memory. A listing filtered to one type costs two queries (the files
    /// query plus one subtype batch) whatever its size, or more only when
    /// chunking splits that one batch across several `IN` lists; a mixed
    /// listing costs one query per subtype present, bounded by the five
    /// subtypes that carry `SubtypeMetadata` (Text/Html never register).
    async fn list_filtered_view(
        &self,
        file_type: Option<FileType>,
        state: StateFilter,
        collection_uuid: Option<Uuid>,
        scope: LibraryScope,
    ) -> Result<Vec<FileView>, DomainError>;

    /// The uuid of the library the file `uuid` identifies belongs to, or
    /// `None` when it belongs to none.
    ///
    /// A read of its own so that a single-file view answers membership the
    /// same way a listing does. One shape that means different things
    /// depending on how the caller got there is the kind of difference
    /// nobody notices until it is a bug.
    async fn find_library_uuid(&self, uuid: Uuid) -> Result<Option<Uuid>, DomainError>;

    /// Every active file belonging to the library `uuid` identifies.
    ///
    /// The one listing that does *not* exclude library files — it is the
    /// place they are shown (libraries design section 3). Answers the whole
    /// library rather than one folder: the tree level is worked out from the
    /// paths, and a query per folder would be a round trip for every click
    /// into a course.
    async fn list_in_library(&self, uuid: Uuid) -> Result<Vec<FileView>, DomainError>;

    /// Read the stored subtype metadata for the file identified by `uuid`
    /// (UC-03 single-file view / FR-FC-13). Returns `Ok(None)` when the file
    /// does not exist, when its subtype has no `SubtypeMetadata` (Text/Html),
    /// or when no editable metadata has been written to the subtype row yet.
    async fn find_metadata_by_uuid(
        &self,
        uuid: Uuid,
    ) -> Result<Option<SubtypeMetadata>, DomainError>;

    /// Write an image file's pixel dimensions (issue #44 image slice).
    /// Unlike `update_metadata`, this touches `images.width`/`images.height`
    /// directly — columns `SubtypeMetadata::Image` deliberately excludes
    /// because they are not owner-editable (UC-04). Returns `NotFound` when
    /// no file row carries the UUID, `InvalidInput` when the file is not an
    /// image.
    async fn set_image_dimensions(
        &self,
        uuid: Uuid,
        width: i64,
        height: i64,
    ) -> Result<(), DomainError>;

    /// Read an image file's pixel dimensions, if both are set (issue #44
    /// image slice). `None` when the file doesn't exist, isn't an image, or
    /// either column is still `NULL` (extraction never ran, or found no
    /// dimensions).
    async fn find_image_dimensions(&self, uuid: Uuid) -> Result<Option<(i64, i64)>, DomainError>;

    /// Write a document file's page count (issue #44 document slice).
    /// Unlike `update_metadata`, this touches `documents.page_count`
    /// directly — `SubtypeMetadata::Document` deliberately excludes it
    /// because it is not owner-editable (UC-04). Returns `NotFound` when
    /// no file row carries the UUID, `InvalidInput` when the file is not a
    /// document.
    async fn set_document_page_count(&self, uuid: Uuid, page_count: i64)
        -> Result<(), DomainError>;

    /// Read a document file's page count, if set (issue #44 document
    /// slice). `None` when the file doesn't exist, isn't a document, or
    /// the column is still `NULL` (extraction never ran, or the file was
    /// EPUB — EPUB never sets this).
    async fn find_document_page_count(&self, uuid: Uuid) -> Result<Option<i64>, DomainError>;

    /// Write a video file's duration in seconds (issue #44 video slice).
    /// Unlike `update_metadata`, this touches `video_files.duration_seconds`
    /// directly — `SubtypeMetadata::Video` deliberately excludes it because
    /// it is not owner-editable (UC-04). Returns `NotFound` when no file row
    /// carries the UUID, `InvalidInput` when the file is not a video.
    async fn set_video_duration(
        &self,
        uuid: Uuid,
        duration_seconds: f64,
    ) -> Result<(), DomainError>;

    /// Read a video file's duration in seconds, if set (issue #44 video
    /// slice). `None` when the file doesn't exist, isn't a video, or the
    /// column is still `NULL` (extraction never ran, or found no readable
    /// duration).
    async fn find_video_duration(&self, uuid: Uuid) -> Result<Option<f64>, DomainError>;

    /// Write an audio file's duration in seconds (music enrichment design).
    /// Its own call rather than a `SubtypeMetadata::Audio` field for the same
    /// reason `set_video_duration` is: how long the audio actually runs is a
    /// property of the stream, not a claim the owner may correct (UC-04).
    /// Returns `NotFound` when no file row carries the UUID, `InvalidInput`
    /// when the file is not audio.
    async fn set_audio_duration(
        &self,
        uuid: Uuid,
        duration_seconds: f64,
    ) -> Result<(), DomainError>;

    /// Write a comic file's page count (issue #44 comic slice). Unlike
    /// `update_metadata`, this touches `comic_books.page_count` directly —
    /// `SubtypeMetadata::Comic` deliberately excludes it because it is not
    /// owner-editable (UC-04). Returns `NotFound` when no file row carries
    /// the UUID, `InvalidInput` when the file is not a comic.
    async fn set_comic_page_count(&self, uuid: Uuid, page_count: i64) -> Result<(), DomainError>;

    /// Read a comic file's page count, if set (issue #44 comic slice).
    /// `None` when the file doesn't exist, isn't a comic, or the column is
    /// still `NULL` (extraction never ran, or the archive couldn't be
    /// opened).
    async fn find_comic_page_count(&self, uuid: Uuid) -> Result<Option<i64>, DomainError>;

    /// Rename a file within its current directory (UC-05 / FR-FC-19). Updates
    /// the cataloged `name` and `path` for the file identified by `uuid` to
    /// `new_name` and `new_path`. The caller is responsible for the on-disk
    /// rename (and rolling it back if this call fails); this method only
    /// touches the catalog row so the variant stays unit-testable against a
    /// fake repo with no filesystem.
    ///
    /// Returns `NotFound` when no file carries the UUID and `InvalidInput`
    /// when `new_path` is already cataloged under a different file (AF-02
    /// target-exists is reported as a disk error by the handler; the
    /// repository defends the unique-path invariant separately).
    async fn rename_file(
        &self,
        uuid: Uuid,
        new_name: &str,
        new_path: &str,
    ) -> Result<File, DomainError>;

    /// Soft-delete a file (UC-06 / FR-FC-20). Sets the row's `state` to
    /// `'deleted'` and stamps `deleted_at` with `deleted_at`; the on-disk
    /// file is untouched (only the catalog row changes). The caller is
    /// responsible for confirming the file is not already `deleted` (the
    /// handler rejects that with `InvalidState`); the repository defends the
    /// `NotFound` invariant.
    ///
    /// Returns the re-read `File` (so the caller sees the exact persisted
    /// `state`/`deleted_at`) or `NotFound` when no row carries the UUID.
    async fn soft_delete(&self, uuid: Uuid, deleted_at: DateTime<Utc>)
        -> Result<File, DomainError>;

    /// Restore a soft-deleted file (UC-07 / FR-FC-21). Sets the row's `state`
    /// back to `'active'` and clears `deleted_at`; the on-disk file is
    /// untouched (only the catalog row changes). The caller is responsible
    /// for confirming the file is still restorable (the handler verifies the
    /// retention window has not elapsed and rejects a non-`deleted` row with
    /// `InvalidState`); the repository defends the `NotFound` invariant.
    ///
    /// Returns the re-read `File` (so the caller sees the exact persisted
    /// `state`/`deleted_at`) or `NotFound` when no row carries the UUID.
    async fn restore(&self, uuid: Uuid) -> Result<File, DomainError>;

    /// Hard-purge a file record (UC-08 / FR-FC-22). Permanently removes the
    /// `files` row and its subtype row; the on-disk file is untouched (NFR-07).
    /// The caller is responsible for confirming the file is `deleted` and past
    /// its retention window (the handler enforces both, rejecting otherwise
    /// with `InvalidState`); the repository defends the `NotFound` invariant.
    async fn purge(&self, uuid: Uuid) -> Result<(), DomainError>;

    /// Link the file identified by `uuid` to the collection identified by
    /// `collection_uuid` (UC-13 / FR-CO-05). The caller has already confirmed
    /// both exist and that the collection is `kind = file`.
    async fn set_collection(&self, uuid: Uuid, collection_uuid: Uuid) -> Result<(), DomainError>;

    /// Unlink the file identified by `uuid` from the collection identified by
    /// `collection_uuid` (UC-14 / FR-CO-06). `NotFound` when the file does
    /// not exist or is not currently linked to that collection (UC-14
    /// AF-01) — the two cases are indistinguishable from the caller's
    /// perspective and the specification maps both to the same error.
    async fn clear_collection(&self, uuid: Uuid, collection_uuid: Uuid) -> Result<(), DomainError>;
}

#[derive(Clone)]
pub struct SqliteCatalogRepository {
    pool: SqlitePool,
}

impl SqliteCatalogRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    fn insert_subtype_sql(file_type: FileType) -> &'static str {
        match file_type {
            FileType::Audio => "INSERT INTO audio_files (file_id) VALUES (?)",
            FileType::Video => "INSERT INTO video_files (file_id) VALUES (?)",
            FileType::Html => "INSERT INTO html_pages (file_id) VALUES (?)",
            FileType::Text => "INSERT INTO text_files (file_id) VALUES (?)",
            FileType::Document => "INSERT INTO documents (file_id) VALUES (?)",
            FileType::Comic => "INSERT INTO comic_books (file_id) VALUES (?)",
            FileType::Image => "INSERT INTO images (file_id) VALUES (?)",
        }
    }

    /// The subtype row is deleted explicitly, ahead of the `files` row (UC-08).
    ///
    /// This is belt-and-braces, not a necessity: the subtype tables declare
    /// `FOREIGN KEY (file_id) REFERENCES files (id) ON DELETE CASCADE`, and the
    /// cascade **is** live, because sqlx sets `PRAGMA foreign_keys = ON` on
    /// every connection it opens (pinned by a test in `tests/migrations.rs`).
    ///
    /// The explicit delete stays because it makes the purge self-contained
    /// rather than dependent on a pragma default that sqlx could change. What
    /// it must *not* be read as is evidence that referential cleanup is
    /// automatic in general: `watch_progress`, `reading_progress`, and the two
    /// `collection_id` columns declare no foreign key at all (SQLite cannot add
    /// one via `ALTER TABLE`), so nothing cascades to them and `purge` /
    /// `delete_collection` clear them by hand.
    fn delete_subtype_sql(file_type: FileType) -> &'static str {
        match file_type {
            FileType::Audio => "DELETE FROM audio_files WHERE file_id = ?",
            FileType::Video => "DELETE FROM video_files WHERE file_id = ?",
            FileType::Html => "DELETE FROM html_pages WHERE file_id = ?",
            FileType::Text => "DELETE FROM text_files WHERE file_id = ?",
            FileType::Document => "DELETE FROM documents WHERE file_id = ?",
            FileType::Comic => "DELETE FROM comic_books WHERE file_id = ?",
            FileType::Image => "DELETE FROM images WHERE file_id = ?",
        }
    }

    /// The `FileView` for each of `files`, with its subtype row attached.
    ///
    /// Extracted so the type listing and the library listing share it rather
    /// than growing two copies of the batching — one query per subtype
    /// actually present, chunked, instead of one per file (issue #116 §2).
    /// Two copies of that would drift, and the second one would be the slow
    /// one nobody noticed.
    async fn views_for(
        &self,
        files: Vec<(i64, File, Option<Uuid>)>,
    ) -> Result<Vec<FileView>, DomainError> {
        // Group the internal ids by subtype, so each subtype table the
        // result actually contains is queried exactly once (chunking
        // aside) rather than once per file (issue #116 §2).
        let mut audio_ids = Vec::new();
        let mut video_ids = Vec::new();
        let mut document_ids = Vec::new();
        let mut comic_ids = Vec::new();
        let mut image_ids = Vec::new();
        for (id, file, _) in &files {
            match file.file_type {
                FileType::Audio => audio_ids.push(*id),
                FileType::Video => video_ids.push(*id),
                FileType::Document => document_ids.push(*id),
                FileType::Comic => comic_ids.push(*id),
                FileType::Image => image_ids.push(*id),
                // Text and Html carry no SubtypeMetadata variant and no
                // extracted scalar — nothing to batch (UC-04).
                FileType::Text | FileType::Html => {}
            }
        }

        // Queries 2..N: one batch per subtype present, each chunked at
        // `MAX_SQLITE_PARAMS`. A listing of a single type touches exactly
        // one of these five branches.
        let audio = self.batch_audio(&audio_ids).await?;
        let video = self.batch_video(&video_ids).await?;
        let document = self.batch_document(&document_ids).await?;
        let comic = self.batch_comic(&comic_ids).await?;
        let image = self.batch_image(&image_ids).await?;

        // Stitch each file to its own batch's row in memory. A missing
        // entry (subtype row never written, or — for Text/Html — no batch
        // ran at all) means every `FileView` field beyond `file` stays
        // `None`, matching `get_by_uuid`'s "absent, not failing" contract.
        let views = files
            .into_iter()
            .map(|(id, file, library_uuid)| match file.file_type {
                FileType::Audio => FileView {
                    file,
                    library_uuid,
                    metadata: audio.get(&id).cloned(),
                    width: None,
                    height: None,
                    page_count: None,
                    duration_seconds: None,
                    comic_page_count: None,
                },
                FileType::Video => {
                    let (metadata, duration_seconds) =
                        video.get(&id).cloned().unwrap_or((None, None));
                    FileView {
                        file,
                        library_uuid,
                        metadata,
                        width: None,
                        height: None,
                        page_count: None,
                        duration_seconds,
                        comic_page_count: None,
                    }
                }
                FileType::Document => {
                    let (metadata, page_count) = document.get(&id).cloned().unwrap_or((None, None));
                    FileView {
                        file,
                        library_uuid,
                        metadata,
                        width: None,
                        height: None,
                        page_count,
                        duration_seconds: None,
                        comic_page_count: None,
                    }
                }
                FileType::Comic => {
                    let (metadata, comic_page_count) =
                        comic.get(&id).cloned().unwrap_or((None, None));
                    FileView {
                        file,
                        library_uuid,
                        metadata,
                        width: None,
                        height: None,
                        page_count: None,
                        duration_seconds: None,
                        comic_page_count,
                    }
                }
                FileType::Image => {
                    let (metadata, width, height) =
                        image.get(&id).cloned().unwrap_or((None, None, None));
                    FileView {
                        file,
                        library_uuid,
                        metadata,
                        width,
                        height,
                        page_count: None,
                        duration_seconds: None,
                        comic_page_count: None,
                    }
                }
                FileType::Text | FileType::Html => FileView {
                    file,
                    library_uuid,
                    metadata: None,
                    width: None,
                    height: None,
                    page_count: None,
                    duration_seconds: None,
                    comic_page_count: None,
                },
            })
            .collect();

        Ok(views)
    }

    async fn list_in_library_impl(&self, uuid: Uuid) -> Result<Vec<FileView>, DomainError> {
        let rows = sqlx::query_as::<_, FileRowWithId>(
            "SELECT f.id, f.uuid, f.path, f.name, f.type, f.content_hash, f.state, \
             f.deleted_at, f.indexed_at, f.missing_at, f.size_bytes, f.mtime, \
             f.metadata_version, l.uuid \
             FROM files f \
             JOIN libraries l ON l.id = f.library_id \
             WHERE l.uuid = ? AND f.state = 'active' \
             ORDER BY f.path",
        )
        .bind(uuid.to_string())
        .fetch_all(&self.pool)
        .await?;

        let files: Vec<(i64, File, Option<Uuid>)> = rows
            .into_iter()
            .map(|row| {
                let id = row.0;
                // An unparseable library uuid reads as "no library" rather
                // than failing the listing: the row is still the owner's
                // file, and one bad marker must not cost them the answer.
                let library_uuid = row.13.as_deref().and_then(|s| Uuid::parse_str(s).ok());
                let file_row: FileRow = (
                    row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9, row.10, row.11,
                    row.12,
                );
                parse_file_row(file_row).map(|file| (id, file, library_uuid))
            })
            .collect::<Result<_, _>>()?;

        self.views_for(files).await
    }

    /// Build the ` WHERE …`/` ORDER BY path` suffix shared by
    /// `list_filtered` and `list_filtered_view` — the two queries select
    /// different columns (`list_filtered_view` also needs the internal
    /// `id`, since the subtype batches key on it) but filter identically.
    /// Appended straight after each query's own `SELECT … FROM files`
    /// prefix. Kept as one function so the two queries cannot drift apart:
    /// FR-CO-07's real collection filter, when it lands, edits this once
    /// rather than two call sites that happen to agree today.
    ///
    /// `file_type` and `collection_uuid` only need to be checked for
    /// presence here — the caller binds their actual values in the same
    /// order this appends the placeholders (`type = ?` before the
    /// collection subquery's `?`).
    fn list_filter_where_clause(
        file_type: Option<FileType>,
        state: StateFilter,
        collection_uuid: Option<Uuid>,
        scope: LibraryScope,
    ) -> String {
        // The filters are enumerated (not user strings), so there is no SQL
        // injection surface here — every value below is still bound as a
        // `?` parameter by the caller.
        let mut sql = String::new();
        let mut conj = " WHERE ";
        if file_type.is_some() {
            sql.push_str(conj);
            sql.push_str("type = ?");
            conj = " AND ";
        }
        match state {
            StateFilter::Active => {
                sql.push_str(conj);
                sql.push_str("state = 'active'");
                conj = " AND ";
            }
            StateFilter::Deleted => {
                sql.push_str(conj);
                sql.push_str("state = 'deleted'");
                conj = " AND ";
            }
            StateFilter::All => {}
        }
        if collection_uuid.is_some() {
            sql.push_str(conj);
            sql.push_str("collection_id = (SELECT id FROM collections WHERE uuid = ?)");
            conj = " AND ";
        }

        // Files in a library are browsed as that library's tree and nowhere
        // else (libraries design section 3) — unless the caller has asked
        // for everything the catalog holds.
        //
        // Carried by the filter rather than by which function was called,
        // and defaulting to the exclusion, so that forgetting to say
        // anything gives the type-panel behaviour rather than leaking a
        // course into it. What the exclusion must NOT do is hide these files
        // from callers that are not browsing: this application's search and
        // its deleted-items review are both built from this listing, and an
        // unconditional exclusion made a library's files unfindable and a
        // deleted one unrestorable (FR-FC-38).
        if matches!(scope, LibraryScope::OutsideLibraries) {
            sql.push_str(conj);
            sql.push_str("library_id IS NULL");
        }

        sql.push_str(" ORDER BY path");
        sql
    }

    /// Build a `column IN (?, ?, …)` placeholder list sized to `chunk`. Used
    /// by the `batch_*` helpers below so each chunk of ids gets exactly as
    /// many placeholders as it has elements — never more, which would bind
    /// past the values given, and never fewer, which would drop ids.
    fn in_placeholders(chunk: &[i64]) -> String {
        std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Batch-fetch audio metadata for every id in `ids`, chunked at
    /// `MAX_SQLITE_PARAMS` (issue #116 §2). An id with every editable field
    /// `NULL` — the indexer inserts an empty subtype row for every file, so
    /// a row existing is not enough — is omitted from the map rather than
    /// mapped to an empty `SubtypeMetadata`, matching `find_metadata_by_uuid`.
    /// An id with no `audio_files` row at all (should not happen while the
    /// indexer keeps the two in lockstep, but the caller does not rely on
    /// that) is likewise simply absent.
    async fn batch_audio(&self, ids: &[i64]) -> Result<HashMap<i64, SubtypeMetadata>, DomainError> {
        let mut out = HashMap::new();
        for chunk in ids.chunks(MAX_SQLITE_PARAMS) {
            let sql = format!(
                "SELECT file_id, title, artist, album, year, genre, track, album_artist \
                 FROM audio_files WHERE file_id IN ({})",
                Self::in_placeholders(chunk)
            );
            let mut query = sqlx::query_as::<_, AudioBatchRow>(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                query = query.bind(id);
            }
            let rows = query.fetch_all(&self.pool).await?;
            for (file_id, title, artist, album, year, genre, track, album_artist) in rows {
                let all_none = title.is_none()
                    && artist.is_none()
                    && album.is_none()
                    && year.is_none()
                    && genre.is_none()
                    && track.is_none()
                    && album_artist.is_none();
                if !all_none {
                    out.insert(
                        file_id,
                        SubtypeMetadata::Audio {
                            title,
                            artist,
                            album,
                            year,
                            genre,
                            track,
                            album_artist,
                        },
                    );
                }
            }
        }
        Ok(out)
    }

    /// Batch-fetch video metadata and duration for every id in `ids`,
    /// chunked at `MAX_SQLITE_PARAMS`. See `batch_audio` for the
    /// all-`NULL`-means-absent rule; here it governs only the `metadata`
    /// half of the pair — `duration_seconds` is reported as read, `NULL`
    /// or not, exactly as `find_video_duration` does.
    #[allow(clippy::type_complexity)]
    async fn batch_video(
        &self,
        ids: &[i64],
    ) -> Result<HashMap<i64, (Option<SubtypeMetadata>, Option<f64>)>, DomainError> {
        let mut out = HashMap::new();
        for chunk in ids.chunks(MAX_SQLITE_PARAMS) {
            let sql = format!(
                "SELECT file_id, title, year, resolution, media_kind, duration_seconds \
                 FROM video_files WHERE file_id IN ({})",
                Self::in_placeholders(chunk)
            );
            let mut query = sqlx::query_as::<_, VideoBatchRow>(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                query = query.bind(id);
            }
            let rows = query.fetch_all(&self.pool).await?;
            for (file_id, title, year, resolution, media_kind, duration_seconds) in rows {
                let all_none = title.is_none()
                    && year.is_none()
                    && resolution.is_none()
                    && media_kind.is_none();
                let metadata = (!all_none).then_some(SubtypeMetadata::Video {
                    title,
                    year,
                    resolution,
                    media_kind: media_kind.and_then(|m| MediaKind::parse(&m)),
                });
                out.insert(file_id, (metadata, duration_seconds));
            }
        }
        Ok(out)
    }

    /// Batch-fetch document metadata and page count for every id in `ids`,
    /// chunked at `MAX_SQLITE_PARAMS`. See `batch_video` for the pairing
    /// rule.
    #[allow(clippy::type_complexity)]
    async fn batch_document(
        &self,
        ids: &[i64],
    ) -> Result<HashMap<i64, (Option<SubtypeMetadata>, Option<i64>)>, DomainError> {
        let mut out = HashMap::new();
        for chunk in ids.chunks(MAX_SQLITE_PARAMS) {
            let sql = format!(
                "SELECT file_id, title, author, year, format_kind, page_count FROM documents \
                 WHERE file_id IN ({})",
                Self::in_placeholders(chunk)
            );
            let mut query = sqlx::query_as::<_, DocumentBatchRow>(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                query = query.bind(id);
            }
            let rows = query.fetch_all(&self.pool).await?;
            for (file_id, title, author, year, format_kind, page_count) in rows {
                let all_none =
                    title.is_none() && author.is_none() && year.is_none() && format_kind.is_none();
                let metadata = (!all_none).then_some(SubtypeMetadata::Document {
                    title,
                    author,
                    year,
                    format_kind: format_kind.and_then(|f| FormatKind::parse(&f)),
                });
                out.insert(file_id, (metadata, page_count));
            }
        }
        Ok(out)
    }

    /// Batch-fetch comic metadata and page count for every id in `ids`,
    /// chunked at `MAX_SQLITE_PARAMS`. See `batch_video` for the pairing
    /// rule.
    #[allow(clippy::type_complexity)]
    async fn batch_comic(
        &self,
        ids: &[i64],
    ) -> Result<HashMap<i64, (Option<SubtypeMetadata>, Option<i64>)>, DomainError> {
        let mut out = HashMap::new();
        for chunk in ids.chunks(MAX_SQLITE_PARAMS) {
            let sql = format!(
                "SELECT file_id, title, series, issue_number, page_count FROM comic_books \
                 WHERE file_id IN ({})",
                Self::in_placeholders(chunk)
            );
            let mut query = sqlx::query_as::<_, ComicBatchRow>(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                query = query.bind(id);
            }
            let rows = query.fetch_all(&self.pool).await?;
            for (file_id, title, series, issue_number, page_count) in rows {
                let all_none = title.is_none() && series.is_none() && issue_number.is_none();
                let metadata = (!all_none).then_some(SubtypeMetadata::Comic {
                    title,
                    series,
                    issue_number,
                });
                out.insert(file_id, (metadata, page_count));
            }
        }
        Ok(out)
    }

    /// Batch-fetch image metadata and pixel dimensions for every id in
    /// `ids`, chunked at `MAX_SQLITE_PARAMS`. See `batch_video` for the
    /// pairing rule; unlike the scalar pairs above, a dimension is only
    /// reported when *both* `width` and `height` are set, matching
    /// `find_image_dimensions`.
    #[allow(clippy::type_complexity)]
    async fn batch_image(
        &self,
        ids: &[i64],
    ) -> Result<HashMap<i64, (Option<SubtypeMetadata>, Option<i64>, Option<i64>)>, DomainError>
    {
        let mut out = HashMap::new();
        for chunk in ids.chunks(MAX_SQLITE_PARAMS) {
            let sql = format!(
                "SELECT file_id, title, caption, width, height FROM images \
                 WHERE file_id IN ({})",
                Self::in_placeholders(chunk)
            );
            let mut query = sqlx::query_as::<_, ImageBatchRow>(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                query = query.bind(id);
            }
            let rows = query.fetch_all(&self.pool).await?;
            for (file_id, title, caption, width, height) in rows {
                let all_none = title.is_none() && caption.is_none();
                let metadata = (!all_none).then_some(SubtypeMetadata::Image { title, caption });
                let (width, height) = match (width, height) {
                    (Some(w), Some(h)) => (Some(w), Some(h)),
                    _ => (None, None),
                };
                out.insert(file_id, (metadata, width, height));
            }
        }
        Ok(out)
    }
}

/// Parse the stored `type` discriminator. A value outside the enum means the
/// row violates the schema's CHECK constraint — that is corruption, not a
/// `text` file, so it is reported rather than guessed at.
fn parse_type_str(s: &str) -> Result<FileType, DomainError> {
    match s {
        "audio" => Ok(FileType::Audio),
        "video" => Ok(FileType::Video),
        "html" => Ok(FileType::Html),
        "text" => Ok(FileType::Text),
        "document" => Ok(FileType::Document),
        "comic" => Ok(FileType::Comic),
        "image" => Ok(FileType::Image),
        other => Err(DomainError::internal(format!(
            "corrupt catalog row: unknown file type {other:?}"
        ))),
    }
}

/// A `files` row as selected by every catalog read, in column order:
/// uuid, path, name, type, content_hash, state, deleted_at, indexed_at,
/// missing_at, size_bytes, mtime, metadata_version. Named so the four read
/// paths share one shape and `parse_file_row` has a single source of truth
/// for it. `content_hash` is nullable (Task 3): `None` means "not computed
/// yet".
type FileRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<i64>,
    Option<String>,
    i64,
);

/// The editable columns of each subtype row, as selected by
/// `find_metadata_by_uuid`.
type AudioRow = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<String>,
    Option<i64>,
    Option<String>,
);
type VideoRow = (Option<String>, Option<i64>, Option<String>, Option<String>);
type DocumentRow = (Option<String>, Option<String>, Option<i64>, Option<String>);
type ComicRow = (Option<String>, Option<String>, Option<i64>);
type ImageRow = (Option<String>, Option<String>);

/// A `files` row as selected by `list_filtered_view`, in column order: the
/// internal `id` (the subtype tables' join key, absent from `FileRow`),
/// then every `FileRow` column. `list_filtered_view` needs the internal id
/// to batch the subtype queries; the public `File` it builds from the rest
/// never carries it.
type FileRowWithId = (
    i64,
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<i64>,
    Option<String>,
    i64,
    // The owning library's uuid, appended last so the `FileRow` slice above
    // it keeps its positions.
    Option<String>,
);

/// The editable columns of each subtype row plus the type's own extracted
/// scalar (see `FileView`'s field docs), as selected in one batched query by
/// `list_filtered_view`'s per-type helpers — `find_metadata_by_uuid` and the
/// `find_*` scalar getters fetch these two pieces separately because
/// `get_by_uuid` only needs them for a single already-known file, but a
/// batch fetching many rows at once has no reason to pay for a second
/// query when both live in the same subtype table.
type AudioBatchRow = (
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<String>,
    Option<i64>,
    Option<String>,
);
type VideoBatchRow = (
    i64,
    Option<String>,
    Option<i64>,
    Option<String>,
    Option<String>,
    Option<f64>,
);
type DocumentBatchRow = (
    i64,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<String>,
    Option<i64>,
);
type ComicBatchRow = (
    i64,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<i64>,
);
type ImageBatchRow = (
    i64,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<i64>,
);

impl CatalogRepository for SqliteCatalogRepository {
    async fn find_by_path(&self, path: &str) -> Result<Option<File>, DomainError> {
        let row: Option<FileRow> = sqlx::query_as(
            "SELECT uuid, path, name, type, content_hash, state, deleted_at, indexed_at, \
             missing_at, size_bytes, mtime, metadata_version FROM files WHERE path = ?",
        )
        .bind(path)
        .fetch_optional(&self.pool)
        .await?;
        row.map(parse_file_row).transpose()
    }

    async fn find_by_uuid(&self, uuid: Uuid) -> Result<Option<File>, DomainError> {
        let row: Option<FileRow> = sqlx::query_as(
            "SELECT uuid, path, name, type, content_hash, state, deleted_at, indexed_at, \
             missing_at, size_bytes, mtime, metadata_version FROM files WHERE uuid = ?",
        )
        .bind(uuid.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.map(parse_file_row).transpose()
    }

    async fn insert_file(&self, new_file: NewFile) -> Result<File, DomainError> {
        let mut tx = self.pool.begin_with(WRITE_TX).await?;

        sqlx::query(
            // `library_id` is resolved here, by the insert itself, rather
            // than claimed afterwards by whatever ran the walk. A file
            // indexed into an existing library belongs to it from the moment
            // it exists — and a rule the database applies is one no future
            // index path can forget to apply (libraries design section 1).
            //
            // The trailing separator is what stops `/library/course` from
            // claiming `/library/course-notes`: a different folder that
            // merely starts with the same letters.
            //
            // Both sides are compared with forward slashes, because on
            // Windows a file's path is `D:\course\class-01\lecture.mp4`
            // and the root it sits under is `D:\course` — and a prefix test
            // that appended `/` to a backslash path matched nothing at all,
            // so a library there silently claimed no file it had.
            //
            // The root is a LIKE *pattern* here, so its own wildcards are
            // escaped before the trailing `/%` is appended — `_` matches any
            // character, and a library at `/media/tv_shows` claimed every
            // file under `/media/tv-shows` without this. `ORDER BY` pairs
            // with the `LIMIT`: overlapping libraries are refused at
            // registration, and if one ever slips through, the file lands in
            // the same one on every insert rather than in whichever row the
            // planner reached first.
            "INSERT INTO files \
             (uuid, path, name, type, content_hash, size_bytes, mtime, state, deleted_at, \
             indexed_at, missing_at, library_id) \
             VALUES (?, ?, ?, ?, ?, ?, ?, 'active', NULL, ?, NULL, \
             (SELECT id FROM libraries \
              WHERE replace(?, '\\', '/') \
                    LIKE (replace(replace(rtrim(replace(root_path, '\\', '/'), '/'), \
                                          '%', '\\%'), '_', '\\_') || '/%') ESCAPE '\\' \
              ORDER BY length(root_path) DESC, id \
              LIMIT 1))",
        )
        .bind(new_file.uuid.to_string())
        .bind(&new_file.path)
        .bind(&new_file.name)
        .bind(new_file.file_type.as_str())
        .bind(new_file.content_hash.as_deref())
        .bind(new_file.size_bytes)
        .bind(new_file.mtime.map(|t| t.to_rfc3339()))
        .bind(new_file.indexed_at.to_rfc3339())
        // The path a second time, for the library subquery above.
        .bind(&new_file.path)
        .execute(&mut *tx)
        .await?;

        let (id,): (i64,) = sqlx::query_as("SELECT last_insert_rowid()")
            .fetch_one(&mut *tx)
            .await?;

        sqlx::query(Self::insert_subtype_sql(new_file.file_type))
            .bind(id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        Ok(File {
            uuid: new_file.uuid,
            path: new_file.path,
            name: new_file.name,
            file_type: new_file.file_type,
            content_hash: new_file.content_hash,
            size_bytes: new_file.size_bytes,
            mtime: new_file.mtime,
            state: FileState::Active,
            deleted_at: None,
            indexed_at: new_file.indexed_at,
            missing_at: None,
            // Nothing has been extracted for this row yet: the caller writes
            // the stamp once extraction has actually run, so a row whose
            // tags could not be read is revisited rather than declared
            // current.
            metadata_version: 0,
        })
    }

    async fn list_all(&self) -> Result<Vec<File>, DomainError> {
        let rows: Vec<FileRow> = sqlx::query_as(
            "SELECT uuid, path, name, type, content_hash, state, deleted_at, indexed_at, \
             missing_at, size_bytes, mtime, metadata_version FROM files ORDER BY path",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(parse_file_row).collect()
    }

    async fn refresh_hash(
        &self,
        path: &str,
        content_hash: &str,
        size_bytes: i64,
        mtime: Option<DateTime<Utc>>,
        indexed_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        sqlx::query(
            "UPDATE files SET content_hash = ?, size_bytes = ?, mtime = ?, \
             indexed_at = ?, missing_at = NULL WHERE path = ?",
        )
        .bind(content_hash)
        .bind(size_bytes)
        .bind(mtime.map(|t| t.to_rfc3339()))
        .bind(indexed_at.to_rfc3339())
        .bind(path)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn refresh_stat(
        &self,
        path: &str,
        size_bytes: i64,
        mtime: Option<DateTime<Utc>>,
        indexed_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        sqlx::query(
            "UPDATE files SET size_bytes = ?, mtime = ?, content_hash = NULL, \
             indexed_at = ?, missing_at = NULL WHERE path = ?",
        )
        .bind(size_bytes)
        .bind(mtime.map(|t| t.to_rfc3339()))
        .bind(indexed_at.to_rfc3339())
        .bind(path)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn mark_missing(&self, path: &str, missing_at: DateTime<Utc>) -> Result<(), DomainError> {
        sqlx::query("UPDATE files SET missing_at = ? WHERE path = ?")
            .bind(missing_at.to_rfc3339())
            .bind(path)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn update_metadata(
        &self,
        uuid: Uuid,
        metadata: &SubtypeMetadata,
    ) -> Result<(), DomainError> {
        let mut tx = self.pool.begin_with(WRITE_TX).await?;

        // Resolve the file's internal id and its type in one transaction so a
        // race with a concurrent delete can't produce a subtype write against a
        // different file.
        let (id, type_str): (i64, String) =
            sqlx::query_as("SELECT id, type FROM files WHERE uuid = ?")
                .bind(uuid.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(DomainError::NotFound)?;

        let actual_type = parse_type_str(&type_str)?;
        if actual_type != metadata.file_type() {
            return Err(DomainError::InvalidInput(
                "metadata does not match file subtype".into(),
            ));
        }

        // Every arm writes exactly the file's own subtype row. The affected-row
        // count is checked after the match: a zero-row UPDATE means the subtype
        // row is missing, which is a broken invariant, not a successful edit.
        let affected = match metadata {
            SubtypeMetadata::Audio {
                title,
                artist,
                album,
                year,
                genre,
                track,
                album_artist,
            } => {
                sqlx::query(
                    "UPDATE audio_files \
                     SET title = ?, artist = ?, album = ?, year = ?, genre = ?, track = ?, \
                     album_artist = ? \
                     WHERE file_id = ?",
                )
                .bind(title)
                .bind(artist)
                .bind(album)
                .bind(*year)
                .bind(genre)
                .bind(*track)
                .bind(album_artist)
                .bind(id)
                .execute(&mut *tx)
                .await?
            }
            SubtypeMetadata::Video {
                title,
                year,
                resolution,
                media_kind,
            } => {
                sqlx::query(
                    "UPDATE video_files \
                     SET title = ?, year = ?, resolution = ?, media_kind = ? \
                     WHERE file_id = ?",
                )
                .bind(title)
                .bind(*year)
                .bind(resolution)
                .bind(media_kind.map(|m| m.as_str()))
                .bind(id)
                .execute(&mut *tx)
                .await?
            }
            SubtypeMetadata::Document {
                title,
                author,
                year,
                format_kind,
            } => {
                sqlx::query(
                    "UPDATE documents \
                     SET title = ?, author = ?, year = ?, format_kind = ? \
                     WHERE file_id = ?",
                )
                .bind(title)
                .bind(author)
                .bind(*year)
                .bind(format_kind.map(|f| f.as_str()))
                .bind(id)
                .execute(&mut *tx)
                .await?
            }
            SubtypeMetadata::Comic {
                title,
                series,
                issue_number,
            } => {
                sqlx::query(
                    "UPDATE comic_books \
                     SET title = ?, series = ?, issue_number = ? \
                     WHERE file_id = ?",
                )
                .bind(title)
                .bind(series)
                .bind(*issue_number)
                .bind(id)
                .execute(&mut *tx)
                .await?
            }
            SubtypeMetadata::Image { title, caption } => {
                sqlx::query("UPDATE images SET title = ?, caption = ? WHERE file_id = ?")
                    .bind(title)
                    .bind(caption)
                    .bind(id)
                    .execute(&mut *tx)
                    .await?
            }
        }
        .rows_affected();

        if affected == 0 {
            return Err(DomainError::internal(format!(
                "subtype row missing for file {uuid} ({})",
                actual_type.as_str()
            )));
        }

        tx.commit().await?;
        Ok(())
    }

    async fn fill_missing_metadata(
        &self,
        uuid: Uuid,
        metadata: &SubtypeMetadata,
    ) -> Result<(), DomainError> {
        let mut tx = self.pool.begin_with(WRITE_TX).await?;

        let (id, type_str): (i64, String) =
            sqlx::query_as("SELECT id, type FROM files WHERE uuid = ?")
                .bind(uuid.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(DomainError::NotFound)?;

        let actual_type = parse_type_str(&type_str)?;
        if actual_type != metadata.file_type() {
            return Err(DomainError::InvalidInput(
                "metadata does not match file subtype".into(),
            ));
        }

        // `COALESCE(column, ?)` on every column, which is the whole
        // difference from `update_metadata` above: a column that already
        // holds something keeps it, and a bind of `None` against an empty
        // column leaves it empty. So a re-extraction can only ever add.
        let affected = match metadata {
            SubtypeMetadata::Audio {
                title,
                artist,
                album,
                year,
                genre,
                track,
                album_artist,
            } => {
                sqlx::query(
                    "UPDATE audio_files \
                     SET title = COALESCE(title, ?), artist = COALESCE(artist, ?), \
                     album = COALESCE(album, ?), year = COALESCE(year, ?), \
                     genre = COALESCE(genre, ?), track = COALESCE(track, ?), \
                     album_artist = COALESCE(album_artist, ?) \
                     WHERE file_id = ?",
                )
                .bind(title)
                .bind(artist)
                .bind(album)
                .bind(*year)
                .bind(genre)
                .bind(*track)
                .bind(album_artist)
                .bind(id)
                .execute(&mut *tx)
                .await?
            }
            SubtypeMetadata::Video {
                title,
                year,
                resolution,
                media_kind,
            } => {
                sqlx::query(
                    "UPDATE video_files \
                     SET title = COALESCE(title, ?), year = COALESCE(year, ?), \
                     resolution = COALESCE(resolution, ?), \
                     media_kind = COALESCE(media_kind, ?) \
                     WHERE file_id = ?",
                )
                .bind(title)
                .bind(*year)
                .bind(resolution)
                .bind(media_kind.map(|m| m.as_str()))
                .bind(id)
                .execute(&mut *tx)
                .await?
            }
            SubtypeMetadata::Document {
                title,
                author,
                year,
                format_kind,
            } => {
                sqlx::query(
                    "UPDATE documents \
                     SET title = COALESCE(title, ?), author = COALESCE(author, ?), \
                     year = COALESCE(year, ?), format_kind = COALESCE(format_kind, ?) \
                     WHERE file_id = ?",
                )
                .bind(title)
                .bind(author)
                .bind(*year)
                .bind(format_kind.map(|f| f.as_str()))
                .bind(id)
                .execute(&mut *tx)
                .await?
            }
            SubtypeMetadata::Comic {
                title,
                series,
                issue_number,
            } => {
                sqlx::query(
                    "UPDATE comic_books \
                     SET title = COALESCE(title, ?), series = COALESCE(series, ?), \
                     issue_number = COALESCE(issue_number, ?) \
                     WHERE file_id = ?",
                )
                .bind(title)
                .bind(series)
                .bind(*issue_number)
                .bind(id)
                .execute(&mut *tx)
                .await?
            }
            SubtypeMetadata::Image { title, caption } => {
                sqlx::query(
                    "UPDATE images \
                     SET title = COALESCE(title, ?), caption = COALESCE(caption, ?) \
                     WHERE file_id = ?",
                )
                .bind(title)
                .bind(caption)
                .bind(id)
                .execute(&mut *tx)
                .await?
            }
        }
        .rows_affected();

        if affected == 0 {
            return Err(DomainError::internal(format!(
                "subtype row missing for file {uuid} ({})",
                actual_type.as_str()
            )));
        }

        tx.commit().await?;
        Ok(())
    }

    async fn set_metadata_version(&self, uuid: Uuid, version: i64) -> Result<(), DomainError> {
        sqlx::query("UPDATE files SET metadata_version = ? WHERE uuid = ?")
            .bind(version)
            .bind(uuid.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn list_filtered(
        &self,
        file_type: Option<FileType>,
        state: StateFilter,
        collection_uuid: Option<Uuid>,
        scope: LibraryScope,
    ) -> Result<Vec<File>, DomainError> {
        // Build the query dynamically based on which filters are active —
        // see `list_filter_where_clause`.
        let base = "SELECT uuid, path, name, type, content_hash, state, deleted_at, \
                    indexed_at, missing_at, size_bytes, mtime, metadata_version \
                    FROM files";
        let mut sql = String::from(base);
        sql.push_str(&Self::list_filter_where_clause(
            file_type,
            state,
            collection_uuid,
            scope,
        ));

        // sqlx 0.9 refuses a runtime-built SQL string unless the caller asserts
        // it was audited. `sql` is assembled only from string literals chosen by
        // the `Option<FileType>` / `StateFilter` / `Option<Uuid>` parameters
        // above — no caller input reaches it, and every value is still a
        // bound `?` parameter.
        let query = sqlx::query_as::<_, FileRow>(sqlx::AssertSqlSafe(sql));
        let query = match file_type {
            Some(t) => query.bind(t.as_str()),
            None => query,
        };
        let query = match collection_uuid {
            Some(u) => query.bind(u.to_string()),
            None => query,
        };

        let rows = query.fetch_all(&self.pool).await?;
        rows.into_iter().map(parse_file_row).collect()
    }

    async fn list_filtered_view(
        &self,
        file_type: Option<FileType>,
        state: StateFilter,
        collection_uuid: Option<Uuid>,
        scope: LibraryScope,
    ) -> Result<Vec<FileView>, DomainError> {
        // Same filter, built by the same helper `list_filtered` uses, with
        // `id` selected alongside the public columns — the subtype batches
        // below key on it, not on `uuid` (see `find_metadata_by_uuid`'s
        // doc comment).
        // The library's uuid comes along by sub-select rather than a join,
        // so the `WHERE` the shared helper builds keeps naming the columns
        // of `files` without a table qualifier.
        let base = "SELECT id, uuid, path, name, type, content_hash, state, deleted_at, \
                    indexed_at, missing_at, size_bytes, mtime, metadata_version, \
                    (SELECT uuid FROM libraries WHERE id = files.library_id) \
                    FROM files";
        let mut sql = String::from(base);
        sql.push_str(&Self::list_filter_where_clause(
            file_type,
            state,
            collection_uuid,
            scope,
        ));

        // See `list_filtered`'s matching comment: `sql` is assembled only
        // from string literals chosen by the enum/Option parameters above.
        let query = sqlx::query_as::<_, FileRowWithId>(sqlx::AssertSqlSafe(sql));
        let query = match file_type {
            Some(t) => query.bind(t.as_str()),
            None => query,
        };
        let query = match collection_uuid {
            Some(u) => query.bind(u.to_string()),
            None => query,
        };

        // Query 1: the files themselves.
        let rows = query.fetch_all(&self.pool).await?;
        let files: Vec<(i64, File, Option<Uuid>)> = rows
            .into_iter()
            .map(|row| {
                let id = row.0;
                // An unparseable library uuid reads as "no library" rather
                // than failing the listing: the row is still the owner's
                // file, and one bad marker must not cost them the answer.
                let library_uuid = row.13.as_deref().and_then(|s| Uuid::parse_str(s).ok());
                let file_row: FileRow = (
                    row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9, row.10, row.11,
                    row.12,
                );
                parse_file_row(file_row).map(|file| (id, file, library_uuid))
            })
            .collect::<Result<_, _>>()?;

        self.views_for(files).await
    }

    async fn find_library_uuid(&self, uuid: Uuid) -> Result<Option<Uuid>, DomainError> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT l.uuid FROM libraries l \
             JOIN files f ON f.library_id = l.id \
             WHERE f.uuid = ?",
        )
        .bind(uuid.to_string())
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.and_then(|(uuid,)| Uuid::parse_str(&uuid).ok()))
    }

    async fn list_in_library(&self, uuid: Uuid) -> Result<Vec<FileView>, DomainError> {
        self.list_in_library_impl(uuid).await
    }

    async fn find_metadata_by_uuid(
        &self,
        uuid: Uuid,
    ) -> Result<Option<SubtypeMetadata>, DomainError> {
        // First resolve the file + its type; None means no such file or a
        // subtype with no SubtypeMetadata variant (Text/Html).
        let row: Option<(i64, String)> =
            sqlx::query_as("SELECT id, type FROM files WHERE uuid = ?")
                .bind(uuid.to_string())
                .fetch_optional(&self.pool)
                .await?;
        let (id, type_str) = match row {
            Some(r) => r,
            None => return Ok(None),
        };
        let file_type = parse_type_str(&type_str)?;

        // "Metadata is present" means at least one editable field is
        // populated. The indexer inserts an empty subtype row (all NULLs)
        // for every file, so a row existing is not enough — we return
        // `None` when every editable field is NULL (no metadata written
        // yet, or UC-04 cleared them all). The client still gets the
        // file's `type` via the `file` object, so it can tell an audio
        // file with no metadata from a text file (which has no
        // SubtypeMetadata variant at all).
        let metadata = match file_type {
            FileType::Audio => {
                let r: Option<AudioRow> = sqlx::query_as(
                    "SELECT title, artist, album, year, genre, track, album_artist \
                     FROM audio_files WHERE file_id = ?",
                )
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
                r.and_then(|(title, artist, album, year, genre, track, album_artist)| {
                    let all_none = title.is_none()
                        && artist.is_none()
                        && album.is_none()
                        && year.is_none()
                        && genre.is_none()
                        && track.is_none()
                        && album_artist.is_none();
                    (!all_none).then_some(SubtypeMetadata::Audio {
                        title,
                        artist,
                        album,
                        year,
                        genre,
                        track,
                        album_artist,
                    })
                })
            }
            FileType::Video => {
                let r: Option<VideoRow> = sqlx::query_as(
                    "SELECT title, year, resolution, media_kind FROM video_files WHERE file_id = ?",
                )
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
                r.and_then(|(title, year, resolution, media_kind)| {
                    let all_none = title.is_none()
                        && year.is_none()
                        && resolution.is_none()
                        && media_kind.is_none();
                    (!all_none).then_some(SubtypeMetadata::Video {
                        title,
                        year,
                        resolution,
                        media_kind: media_kind.and_then(|m| MediaKind::parse(&m)),
                    })
                })
            }
            FileType::Document => {
                let r: Option<DocumentRow> = sqlx::query_as(
                    "SELECT title, author, year, format_kind FROM documents WHERE file_id = ?",
                )
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
                r.and_then(|(title, author, year, format_kind)| {
                    let all_none = title.is_none()
                        && author.is_none()
                        && year.is_none()
                        && format_kind.is_none();
                    (!all_none).then_some(SubtypeMetadata::Document {
                        title,
                        author,
                        year,
                        format_kind: format_kind.and_then(|f| FormatKind::parse(&f)),
                    })
                })
            }
            FileType::Comic => {
                let r: Option<ComicRow> = sqlx::query_as(
                    "SELECT title, series, issue_number FROM comic_books WHERE file_id = ?",
                )
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
                r.and_then(|(title, series, issue_number)| {
                    let all_none = title.is_none() && series.is_none() && issue_number.is_none();
                    (!all_none).then_some(SubtypeMetadata::Comic {
                        title,
                        series,
                        issue_number,
                    })
                })
            }
            FileType::Image => {
                let r: Option<ImageRow> =
                    sqlx::query_as("SELECT title, caption FROM images WHERE file_id = ?")
                        .bind(id)
                        .fetch_optional(&self.pool)
                        .await?;
                r.and_then(|(title, caption)| {
                    let all_none = title.is_none() && caption.is_none();
                    (!all_none).then_some(SubtypeMetadata::Image { title, caption })
                })
            }
            // Text and Html have no editable SubtypeMetadata variant (UC-04).
            FileType::Text | FileType::Html => None,
        };
        Ok(metadata)
    }

    async fn set_image_dimensions(
        &self,
        uuid: Uuid,
        width: i64,
        height: i64,
    ) -> Result<(), DomainError> {
        let mut tx = self.pool.begin_with(WRITE_TX).await?;

        let (id, type_str): (i64, String) =
            sqlx::query_as("SELECT id, type FROM files WHERE uuid = ?")
                .bind(uuid.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(DomainError::NotFound)?;

        let actual_type = parse_type_str(&type_str)?;
        if actual_type != FileType::Image {
            return Err(DomainError::InvalidInput("file is not an image".into()));
        }

        let affected = sqlx::query("UPDATE images SET width = ?, height = ? WHERE file_id = ?")
            .bind(width)
            .bind(height)
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();

        if affected == 0 {
            return Err(DomainError::internal(format!(
                "subtype row missing for file {uuid} (image)"
            )));
        }

        tx.commit().await?;
        Ok(())
    }

    async fn find_image_dimensions(&self, uuid: Uuid) -> Result<Option<(i64, i64)>, DomainError> {
        let row: Option<(i64, String)> =
            sqlx::query_as("SELECT id, type FROM files WHERE uuid = ?")
                .bind(uuid.to_string())
                .fetch_optional(&self.pool)
                .await?;
        let (id, type_str) = match row {
            Some(r) => r,
            None => return Ok(None),
        };
        if parse_type_str(&type_str)? != FileType::Image {
            return Ok(None);
        }

        let dims: Option<(Option<i64>, Option<i64>)> =
            sqlx::query_as("SELECT width, height FROM images WHERE file_id = ?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;

        Ok(dims.and_then(|(w, h)| match (w, h) {
            (Some(w), Some(h)) => Some((w, h)),
            _ => None,
        }))
    }

    async fn set_document_page_count(
        &self,
        uuid: Uuid,
        page_count: i64,
    ) -> Result<(), DomainError> {
        let mut tx = self.pool.begin_with(WRITE_TX).await?;

        let (id, type_str): (i64, String) =
            sqlx::query_as("SELECT id, type FROM files WHERE uuid = ?")
                .bind(uuid.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(DomainError::NotFound)?;

        let actual_type = parse_type_str(&type_str)?;
        if actual_type != FileType::Document {
            return Err(DomainError::InvalidInput("file is not a document".into()));
        }

        let affected = sqlx::query("UPDATE documents SET page_count = ? WHERE file_id = ?")
            .bind(page_count)
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();

        if affected == 0 {
            return Err(DomainError::internal(format!(
                "subtype row missing for file {uuid} (document)"
            )));
        }

        tx.commit().await?;
        Ok(())
    }

    async fn find_document_page_count(&self, uuid: Uuid) -> Result<Option<i64>, DomainError> {
        let row: Option<(i64, String)> =
            sqlx::query_as("SELECT id, type FROM files WHERE uuid = ?")
                .bind(uuid.to_string())
                .fetch_optional(&self.pool)
                .await?;
        let (id, type_str) = match row {
            Some(r) => r,
            None => return Ok(None),
        };
        if parse_type_str(&type_str)? != FileType::Document {
            return Ok(None);
        }

        let row: Option<(Option<i64>,)> =
            sqlx::query_as("SELECT page_count FROM documents WHERE file_id = ?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;

        Ok(row.and_then(|(pc,)| pc))
    }

    async fn set_video_duration(
        &self,
        uuid: Uuid,
        duration_seconds: f64,
    ) -> Result<(), DomainError> {
        let mut tx = self.pool.begin_with(WRITE_TX).await?;

        let (id, type_str): (i64, String) =
            sqlx::query_as("SELECT id, type FROM files WHERE uuid = ?")
                .bind(uuid.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(DomainError::NotFound)?;

        let actual_type = parse_type_str(&type_str)?;
        if actual_type != FileType::Video {
            return Err(DomainError::InvalidInput("file is not a video".into()));
        }

        let affected = sqlx::query("UPDATE video_files SET duration_seconds = ? WHERE file_id = ?")
            .bind(duration_seconds)
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();

        if affected == 0 {
            return Err(DomainError::internal(format!(
                "subtype row missing for file {uuid} (video)"
            )));
        }

        tx.commit().await?;
        Ok(())
    }

    async fn set_audio_duration(
        &self,
        uuid: Uuid,
        duration_seconds: f64,
    ) -> Result<(), DomainError> {
        let mut tx = self.pool.begin_with(WRITE_TX).await?;

        let (id, type_str): (i64, String) =
            sqlx::query_as("SELECT id, type FROM files WHERE uuid = ?")
                .bind(uuid.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(DomainError::NotFound)?;

        let actual_type = parse_type_str(&type_str)?;
        if actual_type != FileType::Audio {
            return Err(DomainError::InvalidInput("file is not audio".into()));
        }

        let affected = sqlx::query("UPDATE audio_files SET duration_seconds = ? WHERE file_id = ?")
            .bind(duration_seconds)
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();

        if affected == 0 {
            return Err(DomainError::internal(format!(
                "subtype row missing for file {uuid} (audio)"
            )));
        }

        tx.commit().await?;
        Ok(())
    }

    async fn find_video_duration(&self, uuid: Uuid) -> Result<Option<f64>, DomainError> {
        let row: Option<(i64, String)> =
            sqlx::query_as("SELECT id, type FROM files WHERE uuid = ?")
                .bind(uuid.to_string())
                .fetch_optional(&self.pool)
                .await?;
        let (id, type_str) = match row {
            Some(r) => r,
            None => return Ok(None),
        };
        if parse_type_str(&type_str)? != FileType::Video {
            return Ok(None);
        }

        let row: Option<(Option<f64>,)> =
            sqlx::query_as("SELECT duration_seconds FROM video_files WHERE file_id = ?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;

        Ok(row.and_then(|(d,)| d))
    }

    async fn set_comic_page_count(&self, uuid: Uuid, page_count: i64) -> Result<(), DomainError> {
        let mut tx = self.pool.begin_with(WRITE_TX).await?;

        let (id, type_str): (i64, String) =
            sqlx::query_as("SELECT id, type FROM files WHERE uuid = ?")
                .bind(uuid.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(DomainError::NotFound)?;

        let actual_type = parse_type_str(&type_str)?;
        if actual_type != FileType::Comic {
            return Err(DomainError::InvalidInput("file is not a comic".into()));
        }

        let affected = sqlx::query("UPDATE comic_books SET page_count = ? WHERE file_id = ?")
            .bind(page_count)
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();

        if affected == 0 {
            return Err(DomainError::internal(format!(
                "subtype row missing for file {uuid} (comic)"
            )));
        }

        tx.commit().await?;
        Ok(())
    }

    async fn find_comic_page_count(&self, uuid: Uuid) -> Result<Option<i64>, DomainError> {
        let row: Option<(i64, String)> =
            sqlx::query_as("SELECT id, type FROM files WHERE uuid = ?")
                .bind(uuid.to_string())
                .fetch_optional(&self.pool)
                .await?;
        let (id, type_str) = match row {
            Some(r) => r,
            None => return Ok(None),
        };
        if parse_type_str(&type_str)? != FileType::Comic {
            return Ok(None);
        }

        let row: Option<(Option<i64>,)> =
            sqlx::query_as("SELECT page_count FROM comic_books WHERE file_id = ?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;

        Ok(row.and_then(|(pc,)| pc))
    }

    async fn rename_file(
        &self,
        uuid: Uuid,
        new_name: &str,
        new_path: &str,
    ) -> Result<File, DomainError> {
        let mut tx = self.pool.begin_with(WRITE_TX).await?;

        // Resolve the file's internal id so a missing uuid is NotFound, not
        // a zero-row UPDATE that the caller could mistake for success.
        let (id,): (i64,) = sqlx::query_as("SELECT id FROM files WHERE uuid = ?")
            .bind(uuid.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(DomainError::NotFound)?;

        // A UPDATE that moves the row to a path already owned by a different
        // file violates the unique constraint, surfacing as a Database error;
        // the handler maps that to AF-02 (disk-error / target-exists). Within
        // a transaction the row's own path is no collision (same row).
        let result = sqlx::query("UPDATE files SET name = ?, path = ? WHERE id = ?")
            .bind(new_name)
            .bind(new_path)
            .bind(id)
            .execute(&mut *tx)
            .await;

        // sqlx 0.9 surfaces SQLite UNIQUE violations as `Database` errors.
        // Treat the unique-path collision as the invariant failure it is
        // (another file owns the target path) — the handler maps it to AF-02.
        let affected = match result {
            Ok(r) => r.rows_affected(),
            Err(sqlx::Error::Database(_)) => {
                return Err(DomainError::InvalidInput(
                    "target path already cataloged for a different file".into(),
                ))
            }
            Err(e) => return Err(e.into()),
        };
        if affected == 0 {
            return Err(DomainError::internal(format!(
                "rename_file matched zero rows for uuid {uuid}"
            )));
        }

        tx.commit().await?;
        let _ = id;

        // Re-read through `find_by_uuid` so the returned `File` carries the
        // exact persisted values (parsed via the single `parse_file_row`
        // path) — no second source of truth for the row shape.
        self.find_by_uuid(uuid).await?.ok_or_else(|| {
            DomainError::internal(format!(
                "rename_file: row disappeared after update for uuid {uuid}"
            ))
        })
    }

    async fn soft_delete(
        &self,
        uuid: Uuid,
        deleted_at: DateTime<Utc>,
    ) -> Result<File, DomainError> {
        let mut tx = self.pool.begin_with(WRITE_TX).await?;

        // Resolve the file's internal id so a missing uuid is NotFound, not
        // a zero-row UPDATE that the caller could mistake for success.
        let (id,): (i64,) = sqlx::query_as("SELECT id FROM files WHERE uuid = ?")
            .bind(uuid.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(DomainError::NotFound)?;

        let affected =
            sqlx::query("UPDATE files SET state = 'deleted', deleted_at = ? WHERE id = ?")
                .bind(deleted_at.to_rfc3339())
                .bind(id)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        if affected == 0 {
            return Err(DomainError::internal(format!(
                "soft_delete matched zero rows for uuid {uuid}"
            )));
        }

        tx.commit().await?;
        let _ = id;

        // Re-read through `find_by_uuid` so the returned `File` carries the
        // exact persisted values (parsed via the single `parse_file_row`
        // path) — no second source of truth for the row shape.
        self.find_by_uuid(uuid).await?.ok_or_else(|| {
            DomainError::internal(format!(
                "soft_delete: row disappeared after update for uuid {uuid}"
            ))
        })
    }

    async fn restore(&self, uuid: Uuid) -> Result<File, DomainError> {
        let mut tx = self.pool.begin_with(WRITE_TX).await?;

        // Resolve the file's internal id so a missing uuid is NotFound, not
        // a zero-row UPDATE that the caller could mistake for success. This
        // mirrors `soft_delete`: the handler has already verified the row is
        // in `deleted` state and within retention.
        let (id,): (i64,) = sqlx::query_as("SELECT id FROM files WHERE uuid = ?")
            .bind(uuid.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(DomainError::NotFound)?;

        let affected =
            sqlx::query("UPDATE files SET state = 'active', deleted_at = NULL WHERE id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        if affected == 0 {
            return Err(DomainError::internal(format!(
                "restore matched zero rows for uuid {uuid}"
            )));
        }

        tx.commit().await?;
        let _ = id;

        // Re-read through `find_by_uuid` so the returned `File` carries the
        // exact persisted values (parsed via the single `parse_file_row`
        // path) — no second source of truth for the row shape.
        self.find_by_uuid(uuid).await?.ok_or_else(|| {
            DomainError::internal(format!(
                "restore: row disappeared after update for uuid {uuid}"
            ))
        })
    }

    async fn purge(&self, uuid: Uuid) -> Result<(), DomainError> {
        let mut tx = self.pool.begin_with(WRITE_TX).await?;

        // Resolve the file's internal id and type so a missing uuid is
        // NotFound, not a zero-row DELETE the caller could mistake for
        // success. The handler has already verified the row is `deleted`
        // and past retention.
        let (id, type_str): (i64, String) =
            sqlx::query_as("SELECT id, type FROM files WHERE uuid = ?")
                .bind(uuid.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(DomainError::NotFound)?;

        let file_type = parse_type_str(&type_str)?;

        // Unlike `update_metadata`, a zero-row subtype DELETE is *not* an
        // error here. `update_metadata` needs the row to exist because it is
        // writing to it — a missing row means the edit silently went nowhere.
        // Purge only needs the row *gone*, which a zero-row DELETE already
        // guarantees. Failing here would leave the `files` row behind and
        // make the operation permanently unretryable for the very rows whose
        // subtype is already missing.
        sqlx::query(Self::delete_subtype_sql(file_type))
            .bind(id)
            .execute(&mut *tx)
            .await?;

        // The progress rows that tracked this file go with it. Unlike the
        // subtype tables, `watch_progress`, `reading_progress`, and
        // `playlist_entries` declare no foreign key (SQLite cannot add one
        // via `ALTER TABLE`), so the cascade that covers the subtype row does
        // not reach them — see `delete_subtype_sql`. Without these
        // statements a purged video/document/comic/track leaves rows
        // pointing at a `files.id` that no longer exists: invisible to
        // UC-21/UC-27 and the playlist read, which inner-join `files`, but
        // permanently orphaned. A zero-row DELETE is the normal case here,
        // not an error.
        sqlx::query("DELETE FROM watch_progress WHERE video_file_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;

        sqlx::query("DELETE FROM reading_progress WHERE item_file_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;

        // The same obligation for the lyrics fetched against this file
        // (music enrichment design). `track_lyrics` carries no foreign key
        // for the identical reason, so a purge that forgot this would leave
        // a row holding lyrics for a `files.id` that no longer exists -- and
        // a later file reusing that autoincrement id would inherit them,
        // which is worse than an orphan: it is the wrong words shown
        // confidently against the wrong track.
        //
        // `artist_images` is deliberately NOT touched here. It is keyed by
        // artist name, not by file, and one purged track does not mean the
        // artist has left the library -- deleting their photograph because
        // one of their files was purged would throw away a lookup every
        // other track by them still uses.
        sqlx::query("DELETE FROM track_lyrics WHERE file_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;

        // Collect the playlists this file's entries belong to *before*
        // deleting them -- once the rows are gone there is nothing left to
        // read `playlist_id` off of. A file can sit in more than one
        // playlist, and more than once in the same one (no `UNIQUE
        // (playlist_id, file_id)`), so this is a distinct list, not one
        // playlist_id per entry.
        let affected_playlists: Vec<(i64,)> =
            sqlx::query_as("SELECT DISTINCT playlist_id FROM playlist_entries WHERE file_id = ?")
                .bind(id)
                .fetch_all(&mut *tx)
                .await?;

        sqlx::query("DELETE FROM playlist_entries WHERE file_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;

        // The DELETE above can punch one or more holes in each affected
        // playlist's position sequence -- unlike `remove_entry`, which
        // removes exactly one row and can shift everything after it down
        // by one, a purge may remove several entries (the same file
        // appearing more than once, or several purges since the playlist
        // was last touched) at scattered positions. Re-reading each
        // playlist's remaining entries in position order and rewriting
        // them to `0..n-1` closes every gap at once, the same read-move-
        // rewrite approach `PlaylistRepository::move_entry` uses, rather
        // than computing which positions shift by how much. Without this,
        // FR-TR-09's contiguity guarantee holds for every mutation except
        // this one -- positions never return to `0..n-1` on their own, and
        // nothing else in this schema renumbers past what the removal
        // itself already touched.
        for (playlist_id,) in affected_playlists {
            let remaining: Vec<(i64,)> = sqlx::query_as(
                "SELECT id FROM playlist_entries WHERE playlist_id = ? ORDER BY position",
            )
            .bind(playlist_id)
            .fetch_all(&mut *tx)
            .await?;

            for (position, (entry_id,)) in remaining.into_iter().enumerate() {
                sqlx::query("UPDATE playlist_entries SET position = ? WHERE id = ?")
                    .bind(position as i64)
                    .bind(entry_id)
                    .execute(&mut *tx)
                    .await?;
            }
        }

        let affected = sqlx::query("DELETE FROM files WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if affected == 0 {
            return Err(DomainError::internal(format!(
                "purge matched zero rows for uuid {uuid}"
            )));
        }

        tx.commit().await?;
        Ok(())
    }

    async fn set_collection(&self, uuid: Uuid, collection_uuid: Uuid) -> Result<(), DomainError> {
        let affected = sqlx::query(
            "UPDATE files SET collection_id = (SELECT id FROM collections WHERE uuid = ?) \
             WHERE uuid = ?",
        )
        .bind(collection_uuid.to_string())
        .bind(uuid.to_string())
        .execute(&self.pool)
        .await?
        .rows_affected();
        if affected == 0 {
            return Err(DomainError::NotFound);
        }
        Ok(())
    }

    async fn clear_collection(&self, uuid: Uuid, collection_uuid: Uuid) -> Result<(), DomainError> {
        let affected = sqlx::query(
            "UPDATE files SET collection_id = NULL \
             WHERE uuid = ? AND collection_id = (SELECT id FROM collections WHERE uuid = ?)",
        )
        .bind(uuid.to_string())
        .bind(collection_uuid.to_string())
        .execute(&self.pool)
        .await?
        .rows_affected();
        if affected == 0 {
            return Err(DomainError::NotFound);
        }
        Ok(())
    }
}

/// Build a `File` from a catalog row.
///
/// A value the domain cannot represent — an unparseable UUID or timestamp, an
/// unknown `type` — means the row is corrupt. Returning an error surfaces that
/// as a `500`; silently substituting the nil UUID or `text` would hand the
/// caller a plausible-looking record that does not correspond to reality.
/// Nullable columns (`deleted_at`, `missing_at`, `content_hash`) are
/// absent-or-valid: a NULL is legitimately `None`, but a present-and-
/// unparseable value is corruption (`content_hash` has no parse step, so
/// there is nothing to fail — every non-NULL value is accepted as-is).
fn parse_file_row(row: FileRow) -> Result<File, DomainError> {
    let (
        uuid_str,
        path,
        name,
        type_str,
        content_hash,
        state_str,
        deleted_at_str,
        indexed_at_str,
        missing_at_str,
        size_bytes,
        mtime_str,
        metadata_version,
    ) = row;

    let uuid = Uuid::parse_str(&uuid_str).map_err(|_| {
        DomainError::internal(format!(
            "corrupt catalog row: unparseable uuid {uuid_str:?}"
        ))
    })?;
    let file_type = parse_type_str(&type_str)?;
    let state = match state_str.as_str() {
        "active" => FileState::Active,
        "deleted" => FileState::Deleted,
        other => {
            return Err(DomainError::internal(format!(
                "corrupt catalog row: unknown state {other:?}"
            )))
        }
    };
    let indexed_at = parse_timestamp(&indexed_at_str, "indexed_at")?;
    let deleted_at = deleted_at_str
        .map(|s| parse_timestamp(&s, "deleted_at"))
        .transpose()?;
    let missing_at = missing_at_str
        .map(|s| parse_timestamp(&s, "missing_at"))
        .transpose()?;
    let mtime = mtime_str
        .map(|s| parse_timestamp(&s, "mtime"))
        .transpose()?;

    Ok(File {
        uuid,
        path,
        name,
        file_type,
        content_hash,
        size_bytes,
        mtime,
        state,
        deleted_at,
        indexed_at,
        missing_at,
        metadata_version,
    })
}

fn parse_timestamp(value: &str, column: &str) -> Result<DateTime<Utc>, DomainError> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|_| {
            DomainError::internal(format!(
                "corrupt catalog row: unparseable {column} {value:?}"
            ))
        })
}
