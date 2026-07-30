use chrono::{DateTime, Utc};
use sqlx::sqlite::SqlitePool;
use uuid::Uuid;

use crate::catalog::model::{File, FileState, FileType, NewFile};
use crate::errors::DomainError;

/// Catalog repository port. The indexer depends on this trait so its decision
/// logic (skip duplicates, insert) is unit-tested against an in-memory fake
/// with no database (Testing Specification §6.2). The Sqlite implementation
/// persists File records and their subtype rows.
#[allow(async_fn_in_trait)]
pub trait CatalogRepository: Send + Sync {
    async fn find_by_path(&self, path: &str) -> Result<Option<File>, DomainError>;
    async fn insert_file(&self, new_file: NewFile) -> Result<File, DomainError>;
}

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

impl CatalogRepository for SqliteCatalogRepository {
    async fn find_by_path(&self, path: &str) -> Result<Option<File>, DomainError> {
        let row: Option<(String, String, String, String, String, String, Option<String>, String)> =
            sqlx::query_as(
                "SELECT uuid, path, name, type, content_hash, state, deleted_at, indexed_at \
                 FROM files WHERE path = ?",
            )
            .bind(path)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(parse_file_row))
    }

    async fn insert_file(&self, new_file: NewFile) -> Result<File, DomainError> {
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "INSERT INTO files \
             (uuid, path, name, type, content_hash, state, deleted_at, indexed_at) \
             VALUES (?, ?, ?, ?, ?, 'active', NULL, ?)",
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
        })
    }
}

fn parse_file_row(
    row: (String, String, String, String, String, String, Option<String>, String),
) -> File {
    let (uuid_str, path, name, type_str, content_hash, state_str, deleted_at_str, indexed_at_str) =
        row;

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

    File {
        uuid,
        path,
        name,
        file_type,
        content_hash,
        state,
        deleted_at,
        indexed_at,
    }
}