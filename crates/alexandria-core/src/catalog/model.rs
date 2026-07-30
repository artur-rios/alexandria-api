use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileType {
    Audio,
    Video,
    Html,
    Text,
    Document,
    Comic,
    Image,
}

impl FileType {
    pub fn as_str(&self) -> &'static str {
        match self {
            FileType::Audio => "audio",
            FileType::Video => "video",
            FileType::Html => "html",
            FileType::Text => "text",
            FileType::Document => "document",
            FileType::Comic => "comic",
            FileType::Image => "image",
        }
    }

    pub fn subtype_table(&self) -> &'static str {
        match self {
            FileType::Audio => "audio_files",
            FileType::Video => "video_files",
            FileType::Html => "html_pages",
            FileType::Text => "text_files",
            FileType::Document => "documents",
            FileType::Comic => "comic_books",
            FileType::Image => "images",
        }
    }
}

impl std::fmt::Display for FileType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileState {
    Active,
    Deleted,
}

impl FileState {
    pub fn as_str(&self) -> &'static str {
        match self {
            FileState::Active => "active",
            FileState::Deleted => "deleted",
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewFile {
    pub uuid: Uuid,
    pub path: String,
    pub name: String,
    pub file_type: FileType,
    pub content_hash: String,
    pub indexed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct File {
    pub uuid: Uuid,
    pub path: String,
    pub name: String,
    pub file_type: FileType,
    pub content_hash: String,
    pub state: FileState,
    pub deleted_at: Option<DateTime<Utc>>,
    pub indexed_at: DateTime<Utc>,
    /// When re-index found the on-disk file gone (FR-FC-11 / UC-02 AF-01).
    /// `None` means the file was present at last refresh. The `state` enum is
    /// unchanged (still `active`/`deleted`); `missing_at` is orthogonal to the
    /// soft-delete lifecycle owned by UC-06/07.
    pub missing_at: Option<DateTime<Utc>>,
}