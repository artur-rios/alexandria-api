/// Tags read from an image file's embedded EXIF data (issue #44 image
/// slice). `width`/`height` are written via `CatalogRepository::set_image_dimensions`
/// (they live outside `SubtypeMetadata::Image`, which only covers the
/// owner-editable `title`/`caption`); `title` is written via the existing
/// `update_metadata` when present. `caption` has no EXIF-native tag and is
/// never populated by extraction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageTags {
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub title: Option<String>,
}

/// Read-only port over an image file's embedded EXIF data (issue #44 image
/// slice). Generic-parameter-injected into `IndexHandler` so the decision
/// logic is unit-tested against a fake with no real file I/O (Testing
/// Specification §6.2); wired with the real `ExifImageMetadataReader` at
/// runtime (services.rs).
#[allow(async_fn_in_trait)]
pub trait ImageMetadataReader: Send + Sync {
    /// Best-effort read of embedded EXIF data. `None` covers both "no EXIF
    /// present" and "couldn't parse this file" — the caller never needs to
    /// tell them apart; extraction failure is never a run failure.
    async fn read(&self, path: &str) -> Option<ImageTags>;
}
