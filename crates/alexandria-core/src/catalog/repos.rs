use chrono::{DateTime, Utc};
use sqlx::sqlite::SqlitePool;
use uuid::Uuid;

use crate::catalog::model::{File, FileState, FileType, NewFile, SubtypeMetadata};
use crate::errors::DomainError;

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
    /// Refresh a file's content hash + `indexed_at` and clear the missing marker
    /// (the on-disk file returned / is present). `state`/`deleted_at` untouched.
    async fn refresh_hash(
        &self,
        path: &str,
        content_hash: &str,
        indexed_at: DateTime<Utc>,
    ) -> Result<(), DomainError>;
    /// Mark a cataloged path's disk file as gone (UC-02 AF-01). Sets
    /// `missing_at`; leaves `state` (soft-delete is UC-06) and `deleted_at`.
    async fn mark_missing(&self, path: &str, missing_at: DateTime<Utc>)
        -> Result<(), DomainError>;
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
}

fn parse_type_str(s: &str) -> FileType {
    match s {
        "audio" => FileType::Audio,
        "video" => FileType::Video,
        "html" => FileType::Html,
        "text" => FileType::Text,
        "document" => FileType::Document,
        "comic" => FileType::Comic,
        "image" => FileType::Image,
        _ => FileType::Text,
    }
}

impl CatalogRepository for SqliteCatalogRepository {
    async fn find_by_path(&self, path: &str) -> Result<Option<File>, DomainError> {
        let row: Option<(
            String,
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            String,
            Option<String>,
        )> = sqlx::query_as(
            "SELECT uuid, path, name, type, content_hash, state, deleted_at, indexed_at, \
             missing_at FROM files WHERE path = ?",
        )
        .bind(path)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(parse_file_row))
    }

    async fn find_by_uuid(&self, uuid: Uuid) -> Result<Option<File>, DomainError> {
        let row: Option<(
            String,
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            String,
            Option<String>,
        )> = sqlx::query_as(
            "SELECT uuid, path, name, type, content_hash, state, deleted_at, indexed_at, \
             missing_at FROM files WHERE uuid = ?",
        )
        .bind(uuid.to_string())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(parse_file_row))
    }

    async fn insert_file(&self, new_file: NewFile) -> Result<File, DomainError> {
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "INSERT INTO files \
             (uuid, path, name, type, content_hash, state, deleted_at, indexed_at, missing_at) \
             VALUES (?, ?, ?, ?, ?, 'active', NULL, ?, NULL)",
        )
        .bind(new_file.uuid.to_string())
        .bind(&new_file.path)
        .bind(&new_file.name)
        .bind(new_file.file_type.as_str())
        .bind(&new_file.content_hash)
        .bind(new_file.indexed_at.to_rfc3339())
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
            state: FileState::Active,
            deleted_at: None,
            indexed_at: new_file.indexed_at,
            missing_at: None,
        })
    }

    async fn list_all(&self) -> Result<Vec<File>, DomainError> {
        let rows: Vec<(
            String,
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            String,
            Option<String>,
        )> = sqlx::query_as(
            "SELECT uuid, path, name, type, content_hash, state, deleted_at, indexed_at, \
             missing_at FROM files ORDER BY path",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(parse_file_row).collect())
    }

    async fn refresh_hash(
        &self,
        path: &str,
        content_hash: &str,
        indexed_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        sqlx::query(
            "UPDATE files SET content_hash = ?, indexed_at = ?, missing_at = NULL \
             WHERE path = ?",
        )
        .bind(content_hash)
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
        let mut tx = self.pool.begin().await?;

        // Resolve the file's internal id and its type in one transaction so a
        // race with a concurrent delete can't produce a subtype write against a
        // different file.
        let (id, type_str): (i64, String) =
            sqlx::query_as("SELECT id, type FROM files WHERE uuid = ?")
                .bind(uuid.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(DomainError::NotFound)?;

        let actual_type = parse_type_str(&type_str);
        if actual_type != metadata.file_type() {
            return Err(DomainError::InvalidInput(
                "metadata does not match file subtype".into(),
            ));
        }

        match metadata {
            SubtypeMetadata::Audio {
                title,
                artist,
                album,
                year,
                genre,
                track,
            } => {
                sqlx::query(
                    "UPDATE audio_files \
                     SET title = ?, artist = ?, album = ?, year = ?, genre = ?, track = ? \
                     WHERE file_id = ?",
                )
                .bind(title)
                .bind(artist)
                .bind(album)
                .bind(*year)
                .bind(genre)
                .bind(*track)
                .bind(id)
                .execute(&mut *tx)
                .await?;
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
                .await?;
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
                .await?;
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
                .await?;
            }
            SubtypeMetadata::Image { title, caption } => {
                sqlx::query("UPDATE images SET title = ?, caption = ? WHERE file_id = ?")
                    .bind(title)
                    .bind(caption)
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
            }
        }

        tx.commit().await?;
        Ok(())
    }
}

fn parse_file_row(
    row: (
        String,
        String,
        String,
        String,
        String,
        String,
        Option<String>,
        String,
        Option<String>,
    ),
) -> File {
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
    ) = row;

    let uuid = Uuid::parse_str(&uuid_str).unwrap_or_default();
    let file_type = match type_str.as_str() {
        "audio" => FileType::Audio,
        "video" => FileType::Video,
        "html" => FileType::Html,
        "text" => FileType::Text,
        "document" => FileType::Document,
        "comic" => FileType::Comic,
        "image" => FileType::Image,
        _ => FileType::Text,
    };
    let state = if state_str == "deleted" {
        FileState::Deleted
    } else {
        FileState::Active
    };
    let indexed_at = DateTime::parse_from_rfc3339(&indexed_at_str)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| DateTime::<Utc>::from_timestamp(0, 0).unwrap());
    let deleted_at = deleted_at_str.and_then(|s| {
        DateTime::parse_from_rfc3339(&s)
            .map(|dt| dt.with_timezone(&Utc))
            .ok()
    });
    let missing_at = missing_at_str.and_then(|s| {
        DateTime::parse_from_rfc3339(&s)
            .map(|dt| dt.with_timezone(&Utc))
            .ok()
    });

    File {
        uuid,
        path,
        name,
        file_type,
        content_hash,
        state,
        deleted_at,
        indexed_at,
        missing_at,
    }
}