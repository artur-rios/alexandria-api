# Design: Extract video metadata during indexing (4th slice of issue #44)

**Date:** 2026-08-06
**Status:** Approved, ready for implementation planning
**Tracks:** [Issue #44](https://github.com/artur-rios/alexandria-api/issues/44) — video scope only

## Context

Issue #44 tracks reading embedded type-specific metadata at index time across
five file-type families. Audio shipped in
[PR #80](https://github.com/artur-rios/alexandria-api/pull/80); image shipped
in [PR #84](https://github.com/artur-rios/alexandria-api/pull/84); document
shipped in [PR #87](https://github.com/artur-rios/alexandria-api/pull/87).
All three established the same pattern: a read-only `<Type>MetadataReader`
trait port wired as a generic `IndexHandler` collaborator, extraction running
once at first index only, extraction failure never failing the run, and —
when a type has data outside the owner-editable `SubtypeMetadata` (image's
`width`/`height`, document's `page_count`) — a narrow new repository
write/read method pair plus a `FileView` field addition to make that data
visible to callers.

This design covers the fourth slice: **video**, via `ffmpeg-next`. Comic
remains a separate follow-up design/plan/implementation cycle.

## Decisions

1. **Format scope: all 10 video extensions, via `ffmpeg-next`.**
   `classify_by_extension` maps mp4/m4v/mkv/avi/mov/webm/mpg/mpeg/wmv/flv to
   `FileType::Video`. Unlike every prior slice — which picked pure-Rust
   libraries and accepted a reduced format subset (document's PDF/EPUB-only,
   leaving mobi/azw/azw3 unextracted) — this slice deliberately uses
   `ffmpeg-next`, FFI bindings to the `ffmpeg` C libraries, to cover every
   video extension the classifier recognizes in one reader. This is a known,
   explicit departure from the "pure Rust only" precedent, made because
   video container fragmentation (MP4 family, Matroska/WebM, AVI, QuickTime,
   MPEG-PS, WMV/ASF) has no realistic pure-Rust coverage beyond MP4/Matroska,
   and full-format coverage was judged worth the added dependency weight.
2. **Build/CI impact: system ffmpeg dev libraries become a build
   requirement.** `ffmpeg-next`'s underlying `ffmpeg-sys-next` crate needs
   `libavformat`, `libavcodec`, `libavutil`, `libavfilter`, `libavdevice`,
   `libswscale`, `libswresample` (dev headers), `pkg-config`, and `clang`
   (for bindgen) present at build time. `.github/workflows/ci.yml` gains an
   `apt-get install -y libavformat-dev libavcodec-dev libavutil-dev
   libavfilter-dev libavdevice-dev libswscale-dev libswresample-dev
   pkg-config clang` step before the cargo steps — the first system
   dependency any part of this project has needed. Local development also
   now requires these libraries installed to build `alexandria-core`.
3. **Extracted fields: `title`, `year`, `resolution`, `duration_seconds`.**
   `resolution` is formatted `"{width}x{height}"` (e.g. `"1920x1080"`) from
   the best video stream's dimensions. `title`/`year` come from the
   container's format-level metadata dictionary (e.g. MP4 `©nam`/`©day`,
   Matroska `Title`/`DateUTC`) when present. `media_kind` (movie vs. series)
   is **not** extracted — unlike document's `format_kind`, it isn't
   inferable from the file itself, so it stays owner-only via UC-04,
   exactly as it is today.
4. **`title`/`year`/`resolution` reuse the existing owner-editable columns;
   only `duration_seconds` needs new plumbing.** `video_files` already has
   `title`, `year`, `resolution` columns, all owner-editable via UC-04's
   `SubtypeMetadata::Video`. Extraction writes these through the existing
   `update_metadata`, the same shortcut document's `format_kind` used —
   no new repository method for these three. `duration_seconds` has no
   column today; it needs the same "outside `SubtypeMetadata`" pattern
   image's `width`/`height` and document's `page_count` established. Unlike
   `page_count` (which already existed in the original schema migration),
   `video_files.duration_seconds` does not exist yet anywhere — this is the
   first slice that requires an actual new migration for its extracted-only
   column, rather than reusing a column the schema already had.
5. **`duration_seconds` is `REAL`, fractional.** `ffmpeg-next` exposes
   container duration with sub-second precision; stored and surfaced as
   `Option<f64>` rather than rounded to whole seconds, preserving that
   precision.
6. **Extraction still runs once, at first index only**; `refresh.rs` stays
   untouched.
7. **Extraction failure is still never a run failure.** An unopenable
   container, an unsupported/corrupt codec, or a file with no video stream
   all collapse to `None` — never `Err`, never counted in
   `IndexOutcome::failed`.

## Architecture

### New port: `VideoMetadataReader`

`crates/alexandria-core/src/catalog/video_tags.rs` (new file), mirroring
`audio_tags.rs`/`image_tags.rs`/`document_tags.rs`'s shape:

```rust
pub struct VideoTags {
    pub title: Option<String>,
    pub year: Option<i64>,
    pub resolution: Option<String>,
    pub duration_seconds: Option<f64>,
}

#[allow(async_fn_in_trait)]
pub trait VideoMetadataReader: Send + Sync {
    /// Best-effort read of embedded video metadata. `None` covers "couldn't
    /// open the container", "no video stream", and "no metadata present"
    /// alike — the caller never needs to tell them apart.
    async fn read(&self, path: &str) -> Option<VideoTags>;
}
```

Concrete implementation `FfmpegVideoMetadataReader` opens the file with
`ffmpeg_next::format::input`, reads the best video stream's decoder
parameters for `width`/`height` (formatted into `resolution`), and reads the
input context's `duration()` (converted from `ffmpeg-next`'s internal
time-base units to seconds) and its metadata dictionary for `title`/`date`
tags. Any failure to open, decode stream parameters, or find a video stream
at all collapses to `None`.

### New repository method and `FileView` field addition

A new migration (`crates/alexandria-core/migrations/00000000000010_video_duration.sql`)
adds `video_files.duration_seconds REAL`.

`CatalogRepository` gains:

```rust
/// Write a video file's duration (issue #44 video slice). Unlike
/// `update_metadata`, this touches `video_files.duration_seconds`
/// directly — `SubtypeMetadata::Video` deliberately excludes it because it
/// is not owner-editable (UC-04). Returns `NotFound` when no file row
/// carries the UUID, `InvalidInput` when the file is not a video.
async fn set_video_duration(&self, uuid: Uuid, duration_seconds: f64) -> Result<(), DomainError>;

/// Read a video file's duration, if set (issue #44 video slice). `None`
/// when the file doesn't exist, isn't a video, or the column is still
/// `NULL` (extraction never ran, or found no readable duration).
async fn find_video_duration(&self, uuid: Uuid) -> Result<Option<f64>, DomainError>;
```

`FileView` (`catalog/model.rs`) gains a fourth extraction-related field,
`duration_seconds: Option<f64>`, `None` for every non-video file.
`BrowseFilesHandler::get_by_uuid` (`catalog/queries/browse.rs`) calls
`find_video_duration` alongside its existing calls, only when
`file.file_type == FileType::Video`.

### `IndexHandler` wiring

`IndexHandler<A, R, F, C, M, N, O, P>` gains an 8th generic parameter,
`P: VideoMetadataReader`, alongside audio's `M`, image's `N`, and document's
`O`. `index_entry` gets a parallel `FileType::Video` branch with two
independent, best-effort writes:

```rust
if file_type == FileType::Video {
    if let Some(tags) = self.video_tags.read(&entry.path).await {
        if let Some(duration) = tags.duration_seconds {
            // set_video_duration, warn+swallow on failure — never
            // counted in IndexOutcome::failed.
        }
        if tags.title.is_some() || tags.year.is_some() || tags.resolution.is_some() {
            // update_metadata with SubtypeMetadata::Video{ title, year,
            // resolution, media_kind: None }, warn+swallow on failure.
        }
    }
}
```

Both writes are independent; neither failing blocks the other or fails
indexing. `services.rs` wires the real `FfmpegVideoMetadataReader` alongside
the existing audio/image/document readers.

## Error handling / failure isolation

- `VideoMetadataReader::read` never returns `Err`; every failure mode
  (unopenable container, no video stream, corrupt/unsupported codec,
  missing metadata) collapses to `None`.
- Both repository write failures (`set_video_duration`, `update_metadata`)
  are logged at `warn` and swallowed independently — neither propagates,
  neither is counted as an indexing failure.
- `ffmpeg-next` calls return `Result` rather than panicking on malformed
  input, so no extra guarding beyond the `Option`-collapsing described.

## Testing strategy

1. **Unit tests** (`IndexHandler` against fakes): `FakeVideoMetadataReader`
   (mirrors the prior three fakes, including a `call_count()` for the
   "reader never consulted for non-video files" test) and
   `FakeCatalogRepository` extended with `set_video_duration` + a
   `video_duration_for(uuid)` inspector. Cases: full tags (title + year +
   resolution + duration) → both writes happen; resolution-only (no
   title/year, but resolution alone triggers the metadata write, no
   duration) → only metadata write happens; duration-only → only the
   duration write happens; no metadata at all → neither write; non-video
   file → reader never consulted.
2. **`FfmpegVideoMetadataReader` unit test** — against a real generated
   fixture. `ffmpeg-next` can both decode and encode, so — mirroring
   `lopdf`'s dual read/write role in the document slice — a tiny test video
   (a few frames, one audio-less video stream, a known resolution and
   duration, an embedded title tag) is built with `ffmpeg-next`'s muxer
   API at test time, no checked-in binary.
3. **HTTP/FFI integration + parity** — index a tagged fixture through both
   surfaces, assert `GET /v1/files/{uuid}` (and its FFI equivalent) return
   the extracted `title`/`year`/`resolution`/`durationSeconds` — using the
   same race-free polling pattern (poll on every column the test asserts
   on, not just the first one to land) established by the image slice's
   final review and carried through the document slice.

## Out of scope (this slice)

- Comic (`ComicInfo.xml`) — a separate design/plan/implementation cycle.
- `media_kind` (movie/series) inference — not inferable from the file
  itself, stays owner-only via UC-04 per decision 3.
- Any provenance/re-extraction behavior on refresh — ruled out by decision
  6, consistent with audio, image, and document.
- Thumbnail/frame extraction, subtitle track detection, audio-track
  metadata within a video container, or any per-stream metadata beyond the
  single best video stream's dimensions — none of this is part of
  `SubtypeMetadata::Video` or `FileView` today, and adding it would be a
  separate, larger design decision beyond "pre-fill what the owner would
  otherwise type in."
