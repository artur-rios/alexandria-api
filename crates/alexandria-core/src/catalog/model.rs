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

/// Lifecycle filter for catalog browse queries (UC-03 / FR-FC-12). The
/// default view excludes soft-deleted records (`Active`); the owner may
/// explicitly request `Deleted` (only soft-deleted) or `All` (both).
///
/// Serialized lowercase so the HTTP query-string value (`?state=active`) and
/// the FFI JSON filter body stay at parity with the REST contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StateFilter {
    /// Only `active` records (default — excludes `deleted`).
    #[default]
    Active,
    /// Only `deleted` records (explicitly requested).
    Deleted,
    /// Both `active` and `deleted` records.
    All,
}

impl StateFilter {
    pub fn as_str(&self) -> &'static str {
        match self {
            StateFilter::Active => "active",
            StateFilter::Deleted => "deleted",
            StateFilter::All => "all",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(StateFilter::Active),
            "deleted" => Some(StateFilter::Deleted),
            "all" => Some(StateFilter::All),
            _ => None,
        }
    }
}

/// Response of UC-03 single-file view (FR-FC-13): the file record plus its
/// stored subtype metadata, when the subtype has one. Text and Html files
/// have no `SubtypeMetadata` variant, so `metadata` is `None` for them.
/// Serialized as `{"file": …, "metadata": …}` over both the HTTP and FFI
/// surfaces so the two stay at parity (FR-FC-24 / NFR-09).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileView {
    pub file: File,
    /// `None` when the subtype has no editable metadata (Text/Html), or when
    /// no metadata has been written to the subtype row yet.
    pub metadata: Option<SubtypeMetadata>,
    /// Extracted pixel dimensions (issue #44 image slice). `None` for every
    /// non-image file, and for an image file whose dimensions haven't been
    /// extracted yet. Raw EXIF dimensions — do not account for `Orientation`;
    /// a rotated image's stored width/height may be transposed relative to
    /// how it displays.
    pub width: Option<i64>,
    pub height: Option<i64>,
    /// Extracted page count (issue #44 document slice). `None` for every
    /// non-document file, for a document whose page count hasn't been
    /// extracted yet, and always for EPUB (reflowable text has no fixed
    /// page count).
    pub page_count: Option<i64>,
    /// Extracted duration in seconds (issue #44 video slice). `None` for
    /// every non-video file, and for a video file whose duration hasn't
    /// been extracted yet.
    pub duration_seconds: Option<f64>,
    /// Extracted page count (issue #44 comic slice). `None` for every
    /// non-comic file, and for a comic file whose archive couldn't be
    /// opened or hasn't been extracted yet. Named `comic_page_count`
    /// rather than `page_count` because `FileView` already has a
    /// `page_count` field for the document slice's extracted page count —
    /// the two are never both `Some` for the same file, but sharing one
    /// name across two distinct subtypes' fields would be ambiguous.
    pub comic_page_count: Option<i64>,
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

/// Response of UC-09 purge-on-disk (FR-FC-23): the pre-delete snapshot of the
/// record plus whether an on-disk file was actually present to remove.
/// `disk_file_present: false` is AF-01 — the record is still purged, but the
/// caller is told there was no file on disk to delete.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PurgeOnDiskOutcome {
    pub file: File,
    pub disk_file_present: bool,
}

/// A TextFile's current on-disk content (UC-32 / FR-TX-01).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileContent {
    pub uuid: Uuid,
    pub content: String,
}
