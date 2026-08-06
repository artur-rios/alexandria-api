use crate::catalog::model::FormatKind;

/// Tags read from a document file's embedded metadata (PDF `/Info`
/// dictionary or EPUB OPF metadata — issue #44 document slice).
/// `format_kind` is set unconditionally whenever either format was
/// identified at all — `Book` for PDF, `Ebook` for EPUB — independent of
/// whether `title`/`author`/`year` were found. `page_count` only ever comes
/// from PDF; EPUB (reflowable text, no fixed pages) always leaves it
/// `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocumentTags {
    pub title: Option<String>,
    pub author: Option<String>,
    pub year: Option<i64>,
    pub format_kind: Option<FormatKind>,
    pub page_count: Option<i64>,
}

/// Read-only port over a document file's embedded metadata (issue #44
/// document slice). Generic-parameter-injected into `IndexHandler` so the
/// decision logic is unit-tested against a fake with no real file I/O
/// (Testing Specification §6.2); wired with the real `PdfEpubMetadataReader`
/// at runtime (services.rs).
#[allow(async_fn_in_trait)]
pub trait DocumentMetadataReader: Send + Sync {
    /// Best-effort read of embedded document metadata. `None` covers
    /// "unsupported extension (mobi/azw/azw3)", "no metadata present", and
    /// "couldn't parse this file" alike — the caller never needs to tell
    /// them apart; extraction failure is never a run failure.
    async fn read(&self, path: &str) -> Option<DocumentTags>;
}
