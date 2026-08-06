/// Tags read from a video file's embedded metadata (container-level
/// duration and format metadata dictionary — issue #44 video slice).
/// `resolution` is formatted `"{width}x{height}"` from the best video
/// stream's dimensions (e.g. `"1920x1080"`). There is no `media_kind`
/// field — movie-vs-series is not inferable from the file itself, so
/// extraction never sets it; the field stays owner-only via UC-04.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoTags {
    pub title: Option<String>,
    pub year: Option<i64>,
    pub resolution: Option<String>,
    pub duration_seconds: Option<VideoDuration>,
}

/// Wraps an `f64` so `VideoTags` can derive `PartialEq`/`Eq` (raw `f64`
/// implements neither). Holds a duration in fractional seconds. `Eq` is
/// sound here because a duration read from a real file is always a
/// finite, non-NaN value — this type is not used for arbitrary float
/// arithmetic, only for carrying and comparing an extracted duration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoDuration(pub f64);

impl Eq for VideoDuration {}

#[allow(async_fn_in_trait)]
pub trait VideoMetadataReader: Send + Sync {
    /// Best-effort read of embedded video metadata. `None` covers
    /// "couldn't open the container", "no video stream", and "no metadata
    /// present" alike — the caller never needs to tell them apart.
    async fn read(&self, path: &str) -> Option<VideoTags>;
}
