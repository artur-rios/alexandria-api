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

/// Video `mediaKind` discriminator (FR-FC-15). A video file is either a
/// standalone movie or an episode of a series; the field is editable via
/// UC-04. Serialized lowercase to match the REST/FFI contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Movie,
    Series,
}

impl MediaKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            MediaKind::Movie => "movie",
            MediaKind::Series => "series",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "movie" => Some(MediaKind::Movie),
            "series" => Some(MediaKind::Series),
            _ => None,
        }
    }
}

/// Document `formatKind` discriminator (FR-FC-16). Editable via UC-04.
/// Serialized lowercase to match the REST/FFI contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FormatKind {
    Book,
    Ebook,
}

impl FormatKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            FormatKind::Book => "book",
            FormatKind::Ebook => "ebook",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "book" => Some(FormatKind::Book),
            "ebook" => Some(FormatKind::Ebook),
            _ => None,
        }
    }
}

/// Type-specific metadata for an editable subtype (UC-04 / FR-FC-14..18).
///
/// Only the five subtypes with editable metadata have a variant. Audio/Video/
/// Document/Comic/Image map one-to-one to their subtype table; Text and Html
/// have no editable subtype metadata, so a PATCH against them is rejected
/// (AF-01) at the handler — there is no variant to (de)serialize.
///
/// Every field is `Option<T>`: a PATCH is a **full replace** of the editable
/// subtype columns. A field present in the body is written; a field absent
/// (deserialized as `None`) writes `NULL`. Columns that are not editable here
/// (`episodeCount`, `pageCount`, `width`, `height`, `sourceUrl`, `savedAt`)
/// are never touched.
///
/// The enum is internally tagged by `type` so the REST/FFI JSON body carries
/// the discriminator (`{"type":"audio","title":…}`), which the handler checks
/// against the file's actual `FileType` (AF-01).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SubtypeMetadata {
    Audio {
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        artist: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        album: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        year: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        genre: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        track: Option<i64>,
    },
    Video {
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        year: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        resolution: Option<String>,
        #[serde(rename = "mediaKind", skip_serializing_if = "Option::is_none")]
        media_kind: Option<MediaKind>,
    },
    Document {
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        author: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        year: Option<i64>,
        #[serde(rename = "formatKind", skip_serializing_if = "Option::is_none")]
        format_kind: Option<FormatKind>,
    },
    Comic {
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        series: Option<String>,
        #[serde(rename = "issueNumber", skip_serializing_if = "Option::is_none")]
        issue_number: Option<i64>,
    },
    Image {
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        caption: Option<String>,
    },
}

impl SubtypeMetadata {
    /// The `FileType` this metadata variant belongs to. Used by the handler
    /// to validate the variant matches the file's actual subtype (AF-01).
    pub fn file_type(&self) -> FileType {
        match self {
            SubtypeMetadata::Audio { .. } => FileType::Audio,
            SubtypeMetadata::Video { .. } => FileType::Video,
            SubtypeMetadata::Document { .. } => FileType::Document,
            SubtypeMetadata::Comic { .. } => FileType::Comic,
            SubtypeMetadata::Image { .. } => FileType::Image,
        }
    }
}

/// Response of UC-04 (and later UC-03): the file record plus its updated
/// subtype metadata. Serialized as `{"file": …, "metadata": …}` over both
/// the HTTP and FFI surfaces so the two stay at parity (FR-FC-24 / NFR-09).
#[derive(Debug, Clone, Serialize)]
pub struct FileMetadata {
    pub file: File,
    pub metadata: SubtypeMetadata,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
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