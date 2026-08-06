# Video Metadata Extraction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extract embedded video metadata (title, year, resolution, duration) during first-index, prefilling `SubtypeMetadata::Video`'s owner-editable columns and a new `duration_seconds` column, via a single `ffmpeg-next`-backed reader covering every video extension the classifier recognizes.

**Architecture:** A new `VideoMetadataReader` trait port with a concrete `FfmpegVideoMetadataReader` implementation becomes `IndexHandler`'s 8th generic collaborator. `title`/`year`/`resolution` reuse the existing owner-editable `video_files` columns via `update_metadata`; `duration_seconds` needs a new migration, a new `CatalogRepository::set_video_duration`/`find_video_duration` method pair, and a new `FileView` field — mirroring image's `width`/`height` and document's `page_count`.

**Tech Stack:** Rust, `ffmpeg-next` (FFI bindings to the system ffmpeg C libraries — a first system dependency for this project), sqlx/SQLite, tokio.

## Global Constraints

- Format scope: **all 10 video extensions** `classify_by_extension` maps to `FileType::Video` (mp4, m4v, mkv, avi, mov, webm, mpg, mpeg, wmv, flv) — one reader, `ffmpeg-next`, covers all of them. Unlike prior slices, there is no unsupported-extension subset for video.
- `media_kind` is **never** auto-set — it isn't inferable from the file itself. Every `SubtypeMetadata::Video` write from extraction always sets `media_kind: None`.
- `resolution` is formatted `"{width}x{height}"` (e.g. `"1920x1080"`) from the best video stream's dimensions — a plain string, not structured width/height columns (unlike image).
- `duration_seconds` only ever comes from `ffmpeg-next`'s container-level duration read, stored as `REAL` (fractional seconds), never rounded.
- Extraction runs **once, at first index only**. Never touch `refresh.rs`.
- Extraction failure (unopenable container, no video stream, corrupt/unsupported codec, missing metadata) is **never** a run failure: not counted in `IndexOutcome::failed`, logged at `debug` at most.
- The `duration_seconds` write (`set_video_duration`) and the `title`/`year`/`resolution`/`media_kind` write (`update_metadata`) are **independent** — a failure in one must not block or be conflated with the other, and neither fails indexing.
- The metadata write fires whenever ANY of `title`/`year`/`resolution` is `Some` — the same "not all-empty" gate as audio/image (unlike document, where `format_kind` alone was always present and made the gate trivially true; video's reader never sets a field the caller didn't ask for, so this slice's gate behaves like audio/image's, not document's).
- CI: `.github/workflows/ci.yml` gains a system-dependency install step (ffmpeg dev libraries + pkg-config + clang) before the cargo steps — the first system dependency this project's CI has needed. Every task that touches `alexandria-core` or `alexandria-ffi` compilation must account for this being present in CI but must ALSO be buildable locally by a developer who has the libraries installed — no code should assume a CI-only environment.
- `ffmpeg-next`'s exact method/type names are best-effort based on its documented API at the time of writing; if a name has moved in the resolved version, fix it against `cargo doc -p ffmpeg-next --open` — this is the same situation four already-shipped slices (`lofty`, `kamadak-exif`/`little_exif`, `lopdf`/`epub`/`zip`) handled successfully.
- Every new/changed Rust file must pass `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` before its task is done — report the **exact literal commands**, not narrower or `--check`-only variants (except Task 9's final verification, which explicitly uses `--check` to *verify* rather than reformat).
- Branch: `feature/video-metadata-extraction` off `main`. One PR at the end of Task 10, following this repo's established branch → PR → CI → squash-merge cycle.

---

### Task 1: Install ffmpeg dev libraries in CI

**Files:**
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Produces: a CI environment where `pkg-config --libs libavformat` (and the other ffmpeg libraries) succeeds, so `cargo build`/`cargo test` for a crate depending on `ffmpeg-next` can compile in later tasks.

This is a standalone, independently verifiable task: after this change, CI has the libraries available even though nothing in the workspace depends on `ffmpeg-next` yet. Doing this first means Task 2 (which adds the real dependency) can be verified green in CI from the start, rather than landing a broken CI run and fixing it after the fact.

- [ ] **Step 1: Add the system dependency install step**

In `.github/workflows/ci.yml`, find:

```yaml
      - uses: actions/checkout@v4

      - name: Install the toolchain
        uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy

      - name: Cache cargo registry and build output
        uses: Swatinem/rust-cache@v2
```

Change to:

```yaml
      - uses: actions/checkout@v4

      # ffmpeg-next's underlying ffmpeg-sys-next crate needs the ffmpeg C
      # dev libraries at build time (issue #44 video slice) — the first
      # system dependency this workspace has needed.
      - name: Install ffmpeg dev libraries
        run: |
          sudo apt-get update
          sudo apt-get install -y \
            libavformat-dev libavcodec-dev libavutil-dev \
            libavfilter-dev libavdevice-dev libswscale-dev \
            libswresample-dev pkg-config clang

      - name: Install the toolchain
        uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy

      - name: Cache cargo registry and build output
        uses: Swatinem/rust-cache@v2
```

- [ ] **Step 2: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: install ffmpeg dev libraries for the video metadata slice"
```

There is nothing to run locally to verify this step in isolation — CI itself is the verification, and it will be exercised for real once Task 9's PR runs. Do not attempt to fabricate a local check; report DONE once the commit is made and move on.

---

### Task 2: `VideoTags` type and `VideoMetadataReader` trait

**Files:**
- Create: `crates/alexandria-core/src/catalog/video_tags.rs`
- Modify: `crates/alexandria-core/src/catalog/mod.rs`

**Interfaces:**
- Produces: `pub struct VideoTags { pub title: Option<String>, pub year: Option<i64>, pub resolution: Option<String>, pub duration_seconds: Option<f64> }`
- Produces: `#[allow(async_fn_in_trait)] pub trait VideoMetadataReader: Send + Sync { async fn read(&self, path: &str) -> Option<VideoTags>; }`

Pure logic, no I/O, no new dependency yet — mirrors all three prior slices' Task 1 exactly in shape.

- [ ] **Step 1: Write the file**

Create `crates\alexandria-core\src\catalog\video_tags.rs`:

```rust
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
```

- [ ] **Step 2: Register the module**

In `crates/alexandria-core/src/catalog/mod.rs`, currently:

```rust
pub mod audio_tags;
pub mod classify;
pub mod clock;
pub mod commands;
pub mod document_tags;
pub mod fs;
pub mod image_tags;
pub mod model;
pub mod queries;
pub mod repos;
```

Change to (alphabetical: `video_tags` after `repos`):

```rust
pub mod audio_tags;
pub mod classify;
pub mod clock;
pub mod commands;
pub mod document_tags;
pub mod fs;
pub mod image_tags;
pub mod model;
pub mod queries;
pub mod repos;
pub mod video_tags;
```

- [ ] **Step 3: Confirm it compiles**

Run: `cargo build -p alexandria-core`
Expected: builds successfully (the new module is currently unused, which is fine — nothing references it yet).

- [ ] **Step 4: `fmt` + `clippy`**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/alexandria-core/src/catalog/video_tags.rs crates/alexandria-core/src/catalog/mod.rs
git commit -m "feat: add VideoTags and VideoMetadataReader port"
```

---

### Task 3: `FfmpegVideoMetadataReader` (real implementation)

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Modify: `crates/alexandria-core/Cargo.toml`
- Modify: `crates/alexandria-core/src/catalog/video_tags.rs`

**Interfaces:**
- Consumes: `VideoTags`, `VideoDuration`, `VideoMetadataReader` from Task 2.
- Produces: `#[derive(Debug, Default, Clone, Copy)] pub struct FfmpegVideoMetadataReader;` implementing `VideoMetadataReader`.

This is the riskiest task in the plan: `ffmpeg-next` is a large, FFI-backed crate with a different API shape than every prior slice's pure-Rust libraries, and building a test fixture means muxing a real (tiny) video file with the same library at test time — the most involved fixture-generation this project has needed.

- [ ] **Step 1: Add the dependency**

In `Cargo.toml` (workspace root), `[workspace.dependencies]` — insert `ffmpeg-next` alphabetically between `epub` and `jsonwebtoken`:

```toml
chrono = { version = "0.4", features = ["serde"] }
epub = "2"
ffmpeg-next = "7"
jsonwebtoken = "9"
kamadak-exif = "0.5"
lofty = "0.22"
lopdf = "0.34"
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
```

In `crates/alexandria-core/Cargo.toml`'s `[dependencies]` — insert `ffmpeg-next` alphabetically after `epub`, before `jsonwebtoken`:

```toml
chrono.workspace = true
epub.workspace = true
ffmpeg-next.workspace = true
jsonwebtoken.workspace = true
kamadak-exif.workspace = true
lofty.workspace = true
lopdf.workspace = true
reqwest.workspace = true
```

Run: `cargo build -p alexandria-core`
Expected: builds successfully (requires the ffmpeg dev libraries from Task 1 to be present in this environment — see the note below if this fails locally). `Cargo.lock` updates. If `ffmpeg-next` doesn't resolve exactly as pinned, adjust the version to the latest available 7.x/8.x release and note the change in your report — this is a normal dependency-resolution step, not a plan defect.

**If this build fails locally with a `pkg-config` or missing-library error**: this environment doesn't have the ffmpeg dev libraries Task 1 installs in CI. Install them for your platform (e.g. `apt-get install libavformat-dev libavcodec-dev libavutil-dev libavfilter-dev libavdevice-dev libswscale-dev libswresample-dev pkg-config clang` on Debian/Ubuntu, `brew install ffmpeg pkg-config` on macOS) before continuing — this is expected environment setup for this task, not a task failure to work around.

- [ ] **Step 2: Write the failing test**

Append to `crates/alexandria-core/src/catalog/video_tags.rs`, inside a new `#[cfg(test)] mod tests` block at the end of the file:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal valid MP4 with `ffmpeg-next` itself: a few frames of
    /// a tiny raw video stream, encoded and muxed to a real file, with a
    /// `title`/`date` tag set on the output format context. This is a
    /// real, playable (if trivial) video file — not hand-crafted bytes.
    fn write_minimal_mp4(path: &std::path::Path, title: &str, width: u32, height: u32) {
        ffmpeg_next::init().expect("ffmpeg init");

        let mut octx = ffmpeg_next::format::output(path).expect("create output context");
        octx.set_metadata({
            let mut dict = ffmpeg_next::Dictionary::new();
            dict.set("title", title);
            dict
        });

        let codec = ffmpeg_next::encoder::find(ffmpeg_next::codec::Id::MPEG4)
            .expect("mpeg4 encoder available");
        let mut ost = octx.add_stream(codec).expect("add video stream");
        let mut encoder = ffmpeg_next::codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()
            .expect("video encoder context");
        encoder.set_width(width);
        encoder.set_height(height);
        encoder.set_format(ffmpeg_next::format::Pixel::YUV420P);
        encoder.set_time_base(ffmpeg_next::Rational(1, 25));
        let mut encoder = encoder.open().expect("open encoder");
        ost.set_parameters(&encoder);

        octx.write_header().expect("write header");

        let mut frame = ffmpeg_next::frame::Video::new(
            ffmpeg_next::format::Pixel::YUV420P,
            width,
            height,
        );
        for plane in 0..frame.planes() {
            frame.data_mut(plane).fill(16);
        }

        // 10 frames at 25fps = 0.4s of video, plenty for a duration/
        // resolution/title extraction test.
        for i in 0..10 {
            frame.set_pts(Some(i));
            encoder.send_frame(&frame).expect("send frame");
            let mut packet = ffmpeg_next::Packet::empty();
            while encoder.receive_packet(&mut packet).is_ok() {
                packet.set_stream(0);
                packet.write_interleaved(&mut octx).expect("write packet");
            }
        }
        encoder.send_eof().expect("send eof");
        let mut packet = ffmpeg_next::Packet::empty();
        while encoder.receive_packet(&mut packet).is_ok() {
            packet.set_stream(0);
            packet.write_interleaved(&mut octx).expect("write packet");
        }
        octx.write_trailer().expect("write trailer");
    }

    #[tokio::test]
    async fn given_tagged_mp4_when_read_then_title_resolution_and_duration_extracted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tagged.mp4");
        write_minimal_mp4(&path, "Test Title", 320, 240);

        let reader = FfmpegVideoMetadataReader;
        let tags = reader
            .read(path.to_str().unwrap())
            .await
            .expect("tags extracted");

        assert_eq!(tags.title.as_deref(), Some("Test Title"));
        assert_eq!(tags.resolution.as_deref(), Some("320x240"));
        assert!(
            tags.duration_seconds.is_some(),
            "a real encoded video must report a non-None duration"
        );
        let VideoDuration(seconds) = tags.duration_seconds.unwrap();
        assert!(
            seconds > 0.0,
            "10 frames at 25fps must report a positive duration, got {seconds}"
        );
    }

    #[tokio::test]
    async fn given_missing_file_when_read_then_none_not_panic() {
        let reader = FfmpegVideoMetadataReader;

        let tags = reader.read("/no/such/file.mp4").await;

        assert!(tags.is_none());
    }

    #[tokio::test]
    async fn given_non_video_file_when_read_then_none_not_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("not-a-video.txt");
        std::fs::write(&path, b"just some text, not a container at all")
            .expect("write stub");

        let reader = FfmpegVideoMetadataReader;
        let tags = reader.read(path.to_str().unwrap()).await;

        assert!(tags.is_none());
    }
}
```

If `ffmpeg-next`'s `format::output`/`Dictionary`/`encoder::find`/`codec::context::Context`/`frame::Video`/`Packet` APIs, method names, or exact call sequence for muxing a video don't match the resolved version's actual API, adapt via `cargo doc -p ffmpeg-next --open` — keep the same intent (mux a short, tiny, real video file with a title tag and known dimensions, entirely through `ffmpeg-next`'s own writer path, no checked-in binary). If encoding a real stream proves impractical with the resolved API version, an acceptable fallback is muxing a minimal container with zero video frames but a set `duration`/`title` via the format context directly — note this deviation and its effect on the resolution/duration assertions in your report if you take this path.

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p alexandria-core --lib catalog::video_tags`
Expected: fails to compile — `FfmpegVideoMetadataReader` does not exist yet.

- [ ] **Step 4: Implement `FfmpegVideoMetadataReader`**

Add above the `#[cfg(test)]` block in `video_tags.rs`:

```rust
/// Real video reader covering every extension `classify_by_extension`
/// maps to `FileType::Video` (mp4, m4v, mkv, avi, mov, webm, mpg, mpeg,
/// wmv, flv) via `ffmpeg-next` — unlike every prior slice, no extension
/// subset is left unextracted; ffmpeg's container/codec coverage is broad
/// enough that one reader handles all ten.
#[derive(Debug, Default, Clone, Copy)]
pub struct FfmpegVideoMetadataReader;

impl VideoMetadataReader for FfmpegVideoMetadataReader {
    async fn read(&self, path: &str) -> Option<VideoTags> {
        if ffmpeg_next::init().is_err() {
            return None;
        }

        let ictx = match ffmpeg_next::format::input(path) {
            Ok(ctx) => ctx,
            Err(err) => {
                tracing::debug!(path, error = %err, "could not open video container");
                return None;
            }
        };

        let stream = ictx
            .streams()
            .best(ffmpeg_next::media::Type::Video)?;

        let params = stream.parameters();
        let codec_ctx = ffmpeg_next::codec::context::Context::from_parameters(params).ok()?;
        let decoder = codec_ctx.decoder().video().ok()?;
        let (width, height) = (decoder.width(), decoder.height());
        let resolution = if width > 0 && height > 0 {
            Some(format!("{width}x{height}"))
        } else {
            None
        };

        let duration_seconds = {
            let duration = ictx.duration();
            if duration > 0 {
                Some(VideoDuration(
                    duration as f64 / f64::from(ffmpeg_next::ffi::AV_TIME_BASE),
                ))
            } else {
                None
            }
        };

        let metadata = ictx.metadata();
        let title = metadata
            .get("title")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let year = metadata
            .get("date")
            .and_then(|s| s.get(..4))
            .and_then(|y| y.parse::<i64>().ok());

        Some(VideoTags {
            title,
            year,
            resolution,
            duration_seconds,
        })
    }
}
```

If `ffmpeg_next::init`/`format::input`/`streams().best(...)`/`stream.parameters()`/`codec::context::Context::from_parameters`/`decoder().video()`/`ictx.duration()`/`ffmpeg_next::ffi::AV_TIME_BASE`/`ictx.metadata()` don't match the resolved version's actual API, adapt via `cargo doc -p ffmpeg-next --open`. Keep the same intent: open the container (any failure → `None`), find the best video stream's decoder parameters for width/height, read the container's duration converted to seconds, and read `title`/`date` from the format-level metadata dictionary, trimmed to non-empty strings.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p alexandria-core --lib catalog::video_tags`
Expected: `test result: ok. 3 passed; 0 failed`

- [ ] **Step 6: `fmt` + `clippy`**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings` (the exact, full-workspace commands — nothing downstream of `alexandria-core` has been touched yet, so the whole workspace should still build and lint clean at this point).
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock crates/alexandria-core/Cargo.toml crates/alexandria-core/src/catalog/video_tags.rs
git commit -m "feat: implement FfmpegVideoMetadataReader"
```

---

### Task 4: `FakeVideoMetadataReader` test double

**Files:**
- Modify: `crates/alexandria-core/tests/common/mod.rs`

**Interfaces:**
- Consumes: `VideoMetadataReader`, `VideoTags` from `alexandria_core::catalog::video_tags`.
- Produces: `FakeVideoMetadataReader::new()`, `.seed(path: &str, tags: VideoTags)`, `.call_count()`, implementing `VideoMetadataReader`.

Mirrors `FakeAudioMetadataReader`/`FakeImageMetadataReader`/`FakeDocumentMetadataReader` (already in this file) exactly, including the call-count pattern.

- [ ] **Step 1: Add the fake**

Add this import in `crates/alexandria-core/tests/common/mod.rs`, alongside the existing `alexandria_core::catalog::document_tags::...` / `alexandria_core::catalog::image_tags::...` imports:

```rust
use alexandria_core::catalog::audio_tags::{AudioMetadataReader, AudioTags};
use alexandria_core::catalog::document_tags::{DocumentMetadataReader, DocumentTags};
use alexandria_core::catalog::image_tags::{ImageMetadataReader, ImageTags};
use alexandria_core::catalog::video_tags::{VideoMetadataReader, VideoTags};
```

Append this new fake at the end of the file, after `FakeDocumentMetadataReader`'s `impl DocumentMetadataReader for FakeDocumentMetadataReader` block:

```rust
/// In-memory video reader (issue #44 video slice). `read()` answers
/// `None` for any path with no seeded tags, mirroring "couldn't open
/// container / no video stream / no metadata" — the same outcome
/// `FfmpegVideoMetadataReader` produces for those cases. Also counts
/// calls, so a test can assert the reader was never consulted at all
/// (e.g. for a non-video file).
#[derive(Debug, Default, Clone)]
pub struct FakeVideoMetadataReader {
    tags: Arc<Mutex<HashMap<String, VideoTags>>>,
    call_count: Arc<Mutex<usize>>,
}

impl FakeVideoMetadataReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the tags `read()` returns for `path`.
    pub fn seed(&self, path: &str, tags: VideoTags) -> &Self {
        self.tags.lock().unwrap().insert(path.to_string(), tags);
        self
    }

    /// How many times `read()` has been called.
    pub fn call_count(&self) -> usize {
        *self.call_count.lock().unwrap()
    }
}

impl VideoMetadataReader for FakeVideoMetadataReader {
    async fn read(&self, path: &str) -> Option<VideoTags> {
        *self.call_count.lock().unwrap() += 1;
        self.tags.lock().unwrap().get(path).cloned()
    }
}
```

- [ ] **Step 2: Confirm it compiles**

Run: `cargo test -p alexandria-core --test catalog -- --list`
Expected: compiles cleanly (the fake being unused so far is fine — `common/mod.rs` already has a module-level `#![allow(dead_code)]`).

- [ ] **Step 3: `fmt` + `clippy`**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 4: Commit**

```bash
git add crates/alexandria-core/tests/common/mod.rs
git commit -m "test: add FakeVideoMetadataReader test double"
```

---

### Task 5: Migration + repository methods `set_video_duration` and `find_video_duration`

**Files:**
- Create: `crates/alexandria-core/migrations/00000000000010_video_duration.sql`
- Modify: `crates/alexandria-core/src/catalog/repos.rs`
- Modify: `crates/alexandria-core/tests/common/mod.rs`

**Interfaces:**
- Produces (on `CatalogRepository` trait and its `SqliteCatalogRepository`/`FakeCatalogRepository` implementations):
  - `async fn set_video_duration(&self, uuid: Uuid, duration_seconds: f64) -> Result<(), DomainError>`
  - `async fn find_video_duration(&self, uuid: Uuid) -> Result<Option<f64>, DomainError>`
- Produces on `FakeCatalogRepository`: `pub fn video_duration_for(&self, uuid: Uuid) -> Option<f64>` (test inspector, mirrors the existing `document_page_count_for`).

Mirrors document's `set_document_page_count`/`find_document_page_count` exactly in shape, plus a real schema migration — unlike `page_count` (which already existed in the original schema), `video_files.duration_seconds` does not exist anywhere yet.

- [ ] **Step 1: Write the migration**

Create `crates\alexandria-core\migrations\00000000000010_video_duration.sql`:

```sql
ALTER TABLE video_files ADD COLUMN duration_seconds REAL;
```

- [ ] **Step 2: Add the trait methods**

In `crates/alexandria-core/src/catalog/repos.rs`, in the `CatalogRepository` trait, add these two methods right after the existing `find_document_page_count` method:

```rust
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
```

- [ ] **Step 3: Add the fakes**

In `crates/alexandria-core/tests/common/mod.rs`, add a new field to `FakeCatalogRepository`'s struct definition — it currently ends with:

```rust
    /// Page count last written for `uuid` via `set_document_page_count`
    /// (issue #44 document slice).
    document_page_counts: Arc<Mutex<HashMap<Uuid, i64>>>,
}
```

Change to:

```rust
    /// Page count last written for `uuid` via `set_document_page_count`
    /// (issue #44 document slice).
    document_page_counts: Arc<Mutex<HashMap<Uuid, i64>>>,
    /// Duration (seconds) last written for `uuid` via `set_video_duration`
    /// (issue #44 video slice).
    video_durations: Arc<Mutex<HashMap<Uuid, f64>>>,
}
```

Add an inspector method in `impl FakeCatalogRepository`, right after the existing `document_page_count_for` method:

```rust
    /// Duration (seconds) last written for `uuid` via `set_video_duration`.
    /// `None` means no call has landed for that file yet.
    pub fn video_duration_for(&self, uuid: Uuid) -> Option<f64> {
        self.video_durations.lock().unwrap().get(&uuid).copied()
    }
```

Add the two trait method implementations in `impl CatalogRepository for FakeCatalogRepository`, right after the existing `find_document_page_count` implementation:

```rust
    async fn set_video_duration(
        &self,
        uuid: Uuid,
        duration_seconds: f64,
    ) -> Result<(), DomainError> {
        let files = self.files.lock().unwrap();
        let file = files
            .values()
            .find(|f| f.uuid == uuid)
            .ok_or(DomainError::NotFound)?;
        if file.file_type != alexandria_core::catalog::model::FileType::Video {
            return Err(DomainError::InvalidInput("file is not a video".into()));
        }
        drop(files);
        self.video_durations
            .lock()
            .unwrap()
            .insert(uuid, duration_seconds);
        Ok(())
    }

    async fn find_video_duration(&self, uuid: Uuid) -> Result<Option<f64>, DomainError> {
        let files = self.files.lock().unwrap();
        let file = match files.values().find(|f| f.uuid == uuid) {
            Some(f) => f,
            None => return Ok(None),
        };
        if file.file_type != alexandria_core::catalog::model::FileType::Video {
            return Ok(None);
        }
        drop(files);
        Ok(self.video_durations.lock().unwrap().get(&uuid).copied())
    }
```

- [ ] **Step 4: Confirm the fakes compile**

Run: `cargo test -p alexandria-core --test catalog -- --list`
Expected: compiles cleanly. `cargo build -p alexandria-core` will still fail at this point — the trait now has two new required methods and `SqliteCatalogRepository` doesn't implement them yet. That's expected; the next step fixes it.

- [ ] **Step 5: Implement the real Sqlite methods**

In `crates/alexandria-core/src/catalog/repos.rs`, in `impl CatalogRepository for SqliteCatalogRepository`, add these two methods right after the existing `find_document_page_count` implementation:

```rust
    async fn set_video_duration(
        &self,
        uuid: Uuid,
        duration_seconds: f64,
    ) -> Result<(), DomainError> {
        let mut tx = self.pool.begin().await?;

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

        let affected =
            sqlx::query("UPDATE video_files SET duration_seconds = ? WHERE file_id = ?")
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
```

- [ ] **Step 6: Verify the fakes and the workspace build together**

Run: `cargo build --workspace`
Expected: builds cleanly (this exercises the new migration too, since `SqliteCatalogRepository`'s tests run migrations at connection time).

Run: `cargo test -p alexandria-core --test catalog`
Expected: all existing catalog tests still pass.

- [ ] **Step 7: `fmt` + `clippy`**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/alexandria-core/migrations/00000000000010_video_duration.sql crates/alexandria-core/src/catalog/repos.rs crates/alexandria-core/tests/common/mod.rs
git commit -m "feat: add set_video_duration and find_video_duration to CatalogRepository"
```

---

### Task 6: `FileView` duration_seconds and `BrowseFilesHandler::get_by_uuid` wiring

**Files:**
- Modify: `crates/alexandria-core/src/catalog/model.rs`
- Modify: `crates/alexandria-core/src/catalog/queries/browse.rs`
- Test: `crates/alexandria-core/tests/catalog/browse.rs`

**Interfaces:**
- Consumes: `CatalogRepository::find_video_duration` from Task 5.
- Produces: `FileView { file, metadata, width, height, page_count, duration_seconds: Option<f64> }` (was `{ file, metadata, width, height, page_count }`).

This closes the read-path gap for video, exactly mirroring the document slice's Task 5 (which itself mirrored image's Task 5). No HTTP or FFI code needs to change — both already serialize `FileView` generically.

- [ ] **Step 1: Write the failing test**

`crates/alexandria-core/tests/catalog/browse.rs` already imports `FormatKind`, `MediaKind`, `existing_file_with_hash`, `FakeCatalogRepository`, `FakeAuth`, `handler`, `TOKEN` (used by the existing document tests added in the document slice — search for `given_document_with_extracted_page_count_when_get_by_uuid_then_page_count_present` for the exact pattern to follow). Add these 3 new tests near it, following that same pattern exactly:

```rust
#[tokio::test]
async fn given_video_with_extracted_duration_when_get_by_uuid_then_duration_present() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/movie.mp4", "movie", FileType::Video, "h");
    let uuid = file.uuid;
    repo.seed(file);
    repo.set_video_duration(uuid, 125.5).await.unwrap();

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    assert_eq!(view.duration_seconds, Some(125.5));
}

#[tokio::test]
async fn given_video_with_no_extracted_duration_when_get_by_uuid_then_duration_none() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/movie.mp4", "movie", FileType::Video, "h");
    let uuid = file.uuid;
    repo.seed(file);

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    assert_eq!(view.duration_seconds, None);
}

#[tokio::test]
async fn given_non_video_file_when_get_by_uuid_then_duration_none() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/song.mp3", "song", FileType::Audio, "h");
    let uuid = file.uuid;
    repo.seed(file);

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    assert_eq!(view.duration_seconds, None);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p alexandria-core --test catalog -- browse::`
Expected: fails to compile — `FileView` has no `duration_seconds` field yet, and `get_by_uuid`'s return doesn't set it.

- [ ] **Step 3: Add the field to `FileView`**

In `crates/alexandria-core/src/catalog/model.rs`, find:

```rust
    /// Extracted page count (issue #44 document slice). `None` for every
    /// non-document file, for a document whose page count hasn't been
    /// extracted yet, and always for EPUB (reflowable text has no fixed
    /// page count).
    pub page_count: Option<i64>,
}
```

Change to:

```rust
    /// Extracted page count (issue #44 document slice). `None` for every
    /// non-document file, for a document whose page count hasn't been
    /// extracted yet, and always for EPUB (reflowable text has no fixed
    /// page count).
    pub page_count: Option<i64>,
    /// Extracted duration in seconds (issue #44 video slice). `None` for
    /// every non-video file, and for a video file whose duration hasn't
    /// been extracted yet.
    pub duration_seconds: Option<f64>,
}
```

- [ ] **Step 4: Wire the read in `BrowseFilesHandler::get_by_uuid`**

In `crates/alexandria-core/src/catalog/queries/browse.rs`, find:

```rust
        // Issue #44 document slice: page_count lives outside
        // `SubtypeMetadata` (see `find_document_page_count`'s doc comment),
        // so it's fetched separately and only for document files.
        let page_count = if file.file_type == FileType::Document {
            self.repo.find_document_page_count(uuid).await?
        } else {
            None
        };

        Ok(FileView {
            file,
            metadata,
            width,
            height,
            page_count,
        })
    }
}
```

Change to:

```rust
        // Issue #44 document slice: page_count lives outside
        // `SubtypeMetadata` (see `find_document_page_count`'s doc comment),
        // so it's fetched separately and only for document files.
        let page_count = if file.file_type == FileType::Document {
            self.repo.find_document_page_count(uuid).await?
        } else {
            None
        };

        // Issue #44 video slice: duration_seconds lives outside
        // `SubtypeMetadata` (see `find_video_duration`'s doc comment), so
        // it's fetched separately and only for video files.
        let duration_seconds = if file.file_type == FileType::Video {
            self.repo.find_video_duration(uuid).await?
        } else {
            None
        };

        Ok(FileView {
            file,
            metadata,
            width,
            height,
            page_count,
            duration_seconds,
        })
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p alexandria-core --test catalog -- browse::`
Expected: all pass, including the 3 new ones.

- [ ] **Step 6: Run the full alexandria-core test suite**

Run: `cargo test -p alexandria-core`
Expected: all pass — confirms no other existing test constructs a `FileView` literal that the new required field would break.

- [ ] **Step 7: `fmt` + `clippy`**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/alexandria-core/src/catalog/model.rs crates/alexandria-core/src/catalog/queries/browse.rs crates/alexandria-core/tests/catalog/browse.rs
git commit -m "feat: surface extracted video duration through FileView"
```

---

### Task 7: Wire `VideoMetadataReader` into `IndexHandler`

**Files:**
- Modify: `crates/alexandria-core/src/catalog/commands/index.rs`
- Modify: `crates/alexandria-core/tests/catalog/index.rs`

**Interfaces:**
- Consumes: `VideoMetadataReader`, `VideoTags`, `VideoDuration` from Task 2; `FakeVideoMetadataReader` from Task 4; `CatalogRepository::set_video_duration` from Task 5.
- Produces: `IndexHandler<A, R, F, C, M, N, O, P>` (was `<A, R, F, C, M, N, O>`) — `P: VideoMetadataReader` is the new 8th parameter, with `pub fn new(auth: A, repo: R, fs: F, clock: C, audio_tags: M, image_tags: N, document_tags: O, video_tags: P) -> Self` (was 7 params).

Mirrors the document slice's Task 6 exactly in shape: widening `IndexHandler`'s constructor arity means every existing call site needs the new argument — this file's own tests, plus (in Task 8) `services.rs`. This task deliberately leaves `services.rs` broken; fixing it is Task 8's job.

- [ ] **Step 1: Write the failing tests**

In `crates/alexandria-core/tests/catalog/index.rs`, make these edits.

**1a.** The imports block currently reads:

```rust
use uuid::Uuid;

use alexandria_core::auth::AuthService;
use alexandria_core::catalog::audio_tags::{AudioMetadataReader, AudioTags};
use alexandria_core::catalog::classify::classify_by_extension;
use alexandria_core::catalog::clock::Clock;
use alexandria_core::catalog::commands::index::{IndexHandler, IndexRequest};
use alexandria_core::catalog::document_tags::{DocumentMetadataReader, DocumentTags};
use alexandria_core::catalog::fs::Filesystem;
use alexandria_core::catalog::image_tags::{ImageMetadataReader, ImageTags};
use alexandria_core::catalog::model::{FileType, FormatKind, SubtypeMetadata};
use alexandria_core::catalog::repos::CatalogRepository;
use alexandria_core::errors::DomainError;

use crate::common::{
    existing_file, fixed_clock, now, FakeAudioMetadataReader, FakeAuth, FakeCatalogRepository,
    FakeDocumentMetadataReader, FakeFilesystem, FakeImageMetadataReader,
};
```

Change to:

```rust
use uuid::Uuid;

use alexandria_core::auth::AuthService;
use alexandria_core::catalog::audio_tags::{AudioMetadataReader, AudioTags};
use alexandria_core::catalog::classify::classify_by_extension;
use alexandria_core::catalog::clock::Clock;
use alexandria_core::catalog::commands::index::{IndexHandler, IndexRequest};
use alexandria_core::catalog::document_tags::{DocumentMetadataReader, DocumentTags};
use alexandria_core::catalog::fs::Filesystem;
use alexandria_core::catalog::image_tags::{ImageMetadataReader, ImageTags};
use alexandria_core::catalog::model::{FileType, FormatKind, SubtypeMetadata};
use alexandria_core::catalog::repos::CatalogRepository;
use alexandria_core::catalog::video_tags::{VideoDuration, VideoMetadataReader, VideoTags};
use alexandria_core::errors::DomainError;

use crate::common::{
    existing_file, fixed_clock, now, FakeAudioMetadataReader, FakeAuth, FakeCatalogRepository,
    FakeDocumentMetadataReader, FakeFilesystem, FakeImageMetadataReader, FakeVideoMetadataReader,
};
```

**1b.** Change the `handler` helper function. It currently reads:

```rust
fn handler<A, R, F, C, M, N, O>(
    auth: A,
    repo: R,
    fs: F,
    clock: C,
    audio_tags: M,
    image_tags: N,
    document_tags: O,
) -> IndexHandler<A, R, F, C, M, N, O>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
    C: Clock,
    M: AudioMetadataReader,
    N: ImageMetadataReader,
    O: DocumentMetadataReader,
{
    IndexHandler::new(auth, repo, fs, clock, audio_tags, image_tags, document_tags)
}
```

Change to:

```rust
#[allow(clippy::too_many_arguments)]
fn handler<A, R, F, C, M, N, O, P>(
    auth: A,
    repo: R,
    fs: F,
    clock: C,
    audio_tags: M,
    image_tags: N,
    document_tags: O,
    video_tags: P,
) -> IndexHandler<A, R, F, C, M, N, O, P>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
    C: Clock,
    M: AudioMetadataReader,
    N: ImageMetadataReader,
    O: DocumentMetadataReader,
    P: VideoMetadataReader,
{
    IndexHandler::new(
        auth,
        repo,
        fs,
        clock,
        audio_tags,
        image_tags,
        document_tags,
        video_tags,
    )
}
```

**1c.** Read the actual current content of `crates/alexandria-core/tests/catalog/index.rs` yourself before doing the next edit — the document slice's equivalent task found the plan's call-site-count estimate had drifted from the real file (16 estimated vs. 17 actual), because tasks are added between when a plan is written and when it is executed. Every existing call to `handler(...)` in this file needs an 8th argument, `FakeVideoMetadataReader::new()`, added as the new last argument before the closing `);`. There are two literal shapes to find-and-replace, matching the document slice's own Task 6 pattern:

**Shape 1** — the call's last argument before the closing `);` is the literal `FakeDocumentMetadataReader::new()`. Change every occurrence of:
```
        FakeDocumentMetadataReader::new(),
    );
```
to:
```
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
    );
```

**Shape 2** — the call's last argument before the closing `);` is a named `document_tags` variable (the tests that seed or inspect the document reader, building it as a variable earlier in the test body). Change every occurrence of:
```
        document_tags,
    );
```
to:
```
        document_tags,
        FakeVideoMetadataReader::new(),
    );
```

After both replacements, verify no call site was missed:

Run: `grep -c "handler(" crates/alexandria-core/tests/catalog/index.rs` (or, without a Unix shell, count `handler(` occurrences via your editor's search) — note this count (the number of places `handler(...)` is invoked, including the helper's own definition, which does not count since it is `fn handler(...)`, not a call to it — count only the *call* sites).
Run the equivalent count for `FakeVideoMetadataReader::new()` — expect this to equal the call-site count from above (every pre-existing call site now has exactly one `FakeVideoMetadataReader::new()`, whether inline or — after Step 1d adds new video-specific tests — as a named variable for those new tests specifically).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p alexandria-core --test catalog -- index::`
Expected: fails to compile — constructor arity mismatch, `video_tags` field doesn't exist on `IndexHandler` yet.

- [ ] **Step 3: Implement the change in `index.rs`**

In `crates/alexandria-core/src/catalog/commands/index.rs`, make these exact edits.

Add to the imports:

```rust
use crate::catalog::audio_tags::AudioMetadataReader;
use crate::catalog::document_tags::DocumentMetadataReader;
use crate::catalog::image_tags::ImageMetadataReader;
use crate::catalog::video_tags::VideoMetadataReader;
```

Change the struct + constructor from:

```rust
pub struct IndexHandler<A, R, F, C, M, N, O> {
    auth: A,
    repo: R,
    fs: F,
    clock: C,
    audio_tags: M,
    image_tags: N,
    document_tags: O,
}

impl<A, R, F, C, M, N, O> IndexHandler<A, R, F, C, M, N, O>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
    C: Clock,
    M: AudioMetadataReader,
    N: ImageMetadataReader,
    O: DocumentMetadataReader,
{
    pub fn new(
        auth: A,
        repo: R,
        fs: F,
        clock: C,
        audio_tags: M,
        image_tags: N,
        document_tags: O,
    ) -> Self {
        Self {
            auth,
            repo,
            fs,
            clock,
            audio_tags,
            image_tags,
            document_tags,
        }
    }
```

to:

```rust
pub struct IndexHandler<A, R, F, C, M, N, O, P> {
    auth: A,
    repo: R,
    fs: F,
    clock: C,
    audio_tags: M,
    image_tags: N,
    document_tags: O,
    video_tags: P,
}

impl<A, R, F, C, M, N, O, P> IndexHandler<A, R, F, C, M, N, O, P>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
    C: Clock,
    M: AudioMetadataReader,
    N: ImageMetadataReader,
    O: DocumentMetadataReader,
    P: VideoMetadataReader,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        auth: A,
        repo: R,
        fs: F,
        clock: C,
        audio_tags: M,
        image_tags: N,
        document_tags: O,
        video_tags: P,
    ) -> Self {
        Self {
            auth,
            repo,
            fs,
            clock,
            audio_tags,
            image_tags,
            document_tags,
            video_tags,
        }
    }
```

Add a new `FileType::Video` branch at the end of `index_entry`, right after the existing `FileType::Document` branch and before the final `Ok(true)`:

```rust
        // Best-effort video metadata prefill (issue #44 video slice). Two
        // independent writes: duration (outside SubtypeMetadata, via
        // set_video_duration) and title/year/resolution (via the shared
        // update_metadata, media_kind always None — it is not inferable
        // from the file). Neither write's failure blocks the other or
        // fails indexing.
        if file_type == FileType::Video {
            if let Some(tags) = self.video_tags.read(&entry.path).await {
                if let Some(crate::catalog::video_tags::VideoDuration(duration_seconds)) =
                    tags.duration_seconds
                {
                    if let Err(err) = self
                        .repo
                        .set_video_duration(file.uuid, duration_seconds)
                        .await
                    {
                        tracing::warn!(
                            path = %entry.path,
                            error = %err,
                            "indexed but failed to write extracted video duration"
                        );
                    }
                }
                if tags.title.is_some() || tags.year.is_some() || tags.resolution.is_some() {
                    let metadata = crate::catalog::model::SubtypeMetadata::Video {
                        title: tags.title,
                        year: tags.year,
                        resolution: tags.resolution,
                        media_kind: None,
                    };
                    if let Err(err) = self.repo.update_metadata(file.uuid, &metadata).await {
                        tracing::warn!(
                            path = %entry.path,
                            error = %err,
                            "indexed but failed to write extracted video metadata"
                        );
                    }
                }
            }
        }
        Ok(true)
    }
}
```

(This replaces the file's existing final `Ok(true)\n    }\n}` — the new branch goes immediately before that line, after the existing `FileType::Document` block's closing `}`.)

**1d.** Add these new tests at the end of `crates/alexandria-core/tests/catalog/index.rs` (after the last existing test):

```rust
#[tokio::test]
async fn given_tagged_video_when_execute_then_duration_and_metadata_written() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.mp4", "a.mp4", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let video_tags = FakeVideoMetadataReader::new();
    video_tags.seed(
        "/library/a.mp4",
        VideoTags {
            title: Some("A Movie".to_string()),
            year: Some(2020),
            resolution: Some("1920x1080".to_string()),
            duration_seconds: Some(VideoDuration(125.5)),
        },
    );
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        video_tags,
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.mp4").expect("indexed");
    assert_eq!(repo_handle.video_duration_for(a.uuid), Some(125.5));
    let metadata = repo_handle
        .metadata_for(a.uuid)
        .expect("metadata written from extracted tags");
    assert_eq!(
        metadata,
        SubtypeMetadata::Video {
            title: Some("A Movie".to_string()),
            year: Some(2020),
            resolution: Some("1920x1080".to_string()),
            media_kind: None,
        }
    );
}

#[tokio::test]
async fn given_video_with_duration_but_no_other_fields_when_execute_then_only_duration_written() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.mp4", "a.mp4", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let video_tags = FakeVideoMetadataReader::new();
    video_tags.seed(
        "/library/a.mp4",
        VideoTags {
            title: None,
            year: None,
            resolution: None,
            duration_seconds: Some(VideoDuration(60.0)),
        },
    );
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        video_tags,
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.mp4").expect("indexed");
    assert_eq!(repo_handle.video_duration_for(a.uuid), Some(60.0));
    assert!(
        repo_handle.metadata_for(a.uuid).is_none(),
        "no title/year/resolution extracted means update_metadata is never called"
    );
}

#[tokio::test]
async fn given_video_with_resolution_but_no_duration_when_execute_then_only_metadata_written() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.mp4", "a.mp4", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let video_tags = FakeVideoMetadataReader::new();
    video_tags.seed(
        "/library/a.mp4",
        VideoTags {
            title: None,
            year: None,
            resolution: Some("1280x720".to_string()),
            duration_seconds: None,
        },
    );
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        video_tags,
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.mp4").expect("indexed");
    assert_eq!(
        repo_handle.video_duration_for(a.uuid),
        None,
        "no duration extracted means set_video_duration is never called"
    );
    let metadata = repo_handle
        .metadata_for(a.uuid)
        .expect("resolution written from extracted tags");
    assert_eq!(
        metadata,
        SubtypeMetadata::Video {
            title: None,
            year: None,
            resolution: Some("1280x720".to_string()),
            media_kind: None,
        }
    );
}

#[tokio::test]
async fn given_untagged_video_file_when_execute_then_neither_write_happens() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.mp4", "a.mp4", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
        FakeVideoMetadataReader::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.mp4").expect("indexed");
    assert_eq!(repo_handle.video_duration_for(a.uuid), None);
    assert!(repo_handle.metadata_for(a.uuid).is_none());
}

#[tokio::test]
async fn given_non_video_file_when_execute_then_video_reader_never_consulted() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/notes.md", "notes.md", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let audio_tags = FakeAudioMetadataReader::new();
    let image_tags = FakeImageMetadataReader::new();
    let document_tags = FakeDocumentMetadataReader::new();
    let video_tags = FakeVideoMetadataReader::new();
    let video_tags_handle = video_tags.clone();
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        audio_tags,
        image_tags,
        document_tags,
        video_tags,
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    assert_eq!(
        video_tags_handle.call_count(),
        0,
        "the video reader must not be consulted at all for a non-video file"
    );
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p alexandria-core --test catalog -- index::`
Expected: all pass, including the 5 new tests. `cargo build --workspace` will still fail at this point (`services.rs` not yet updated) — that's expected, exactly as it was during all three prior slices' equivalent task. If you need real GREEN evidence before `services.rs` is fixed, temporarily add an 8th argument to `services.rs`'s `IndexHandler::new(...)` call using `crate::catalog::video_tags::FfmpegVideoMetadataReader` (from Task 3 — works via its fully-qualified path even though `services.rs` doesn't import it yet), confirm GREEN, then `git checkout -- crates/alexandria-core/src/services.rs` before committing.

- [ ] **Step 5: `fmt` + `clippy`**

Run: `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` (the exact, full-workspace commands). Expected: the *only* errors, if any, come from `services.rs`'s now-outdated `IndexHandler::new(...)` call (Task 8's job) — paste the real output in your report showing this.

- [ ] **Step 6: Commit**

```bash
git add crates/alexandria-core/src/catalog/commands/index.rs crates/alexandria-core/tests/catalog/index.rs
git commit -m "feat: extract video metadata into duration and subtype fields on first index"
```

---

### Task 8: Wire `FfmpegVideoMetadataReader` into `services.rs`

**Files:**
- Modify: `crates/alexandria-core/src/services.rs`

**Interfaces:**
- Consumes: `FfmpegVideoMetadataReader` from Task 3, `IndexHandler<A, R, F, C, M, N, O, P>::new` from Task 7.

Fixes the compile break Task 7 deliberately left, exactly mirroring all three prior slices' equivalent task.

- [ ] **Step 1: Add the import**

In `crates/alexandria-core/src/services.rs`, find this block (the file's imports are alphabetically ordered by full path):

```rust
use crate::catalog::queries::browse::BrowseFilesHandler;
use crate::catalog::queries::read_content::ReadTextFileContentHandler;
use crate::catalog::repos::SqliteCatalogRepository;
```

Change to insert the new import between them (`video_tags` sorts alphabetically after `repos`):

```rust
use crate::catalog::queries::browse::BrowseFilesHandler;
use crate::catalog::queries::read_content::ReadTextFileContentHandler;
use crate::catalog::repos::SqliteCatalogRepository;
use crate::catalog::video_tags::FfmpegVideoMetadataReader;
```

- [ ] **Step 2: Update the `DefaultIndexHandler` type alias**

Find:

```rust
pub type DefaultIndexHandler = IndexHandler<
    RuntimeAuthService,
    SqliteCatalogRepository,
    StdFilesystem,
    SystemClock,
    LoftyAudioMetadataReader,
    ExifImageMetadataReader,
    PdfEpubMetadataReader,
>;
```

Change to:

```rust
pub type DefaultIndexHandler = IndexHandler<
    RuntimeAuthService,
    SqliteCatalogRepository,
    StdFilesystem,
    SystemClock,
    LoftyAudioMetadataReader,
    ExifImageMetadataReader,
    PdfEpubMetadataReader,
    FfmpegVideoMetadataReader,
>;
```

- [ ] **Step 3: Update the construction site**

Find:

```rust
    let audio_tags = LoftyAudioMetadataReader;
    let image_tags = ExifImageMetadataReader;
    let document_tags = PdfEpubMetadataReader;
    let index_handler = Arc::new(IndexHandler::new(
        auth.clone(),
        repo.clone(),
        fs,
        clock,
        audio_tags,
        image_tags,
        document_tags,
    ));
```

Change to:

```rust
    let audio_tags = LoftyAudioMetadataReader;
    let image_tags = ExifImageMetadataReader;
    let document_tags = PdfEpubMetadataReader;
    let video_tags = FfmpegVideoMetadataReader;
    let index_handler = Arc::new(IndexHandler::new(
        auth.clone(),
        repo.clone(),
        fs,
        clock,
        audio_tags,
        image_tags,
        document_tags,
        video_tags,
    ));
```

- [ ] **Step 4: Build and run the full workspace test suite**

Run: `cargo build --workspace`
Expected: builds cleanly.

Run: `cargo test --workspace`
Expected: every test passes, `0 failed` across every crate.

- [ ] **Step 5: `fmt` + `clippy`**

Run: `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` (the exact literal commands). Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/alexandria-core/src/services.rs
git commit -m "feat: wire FfmpegVideoMetadataReader into DefaultIndexHandler"
```

---

### Task 9: HTTP/FFI integration + parity test

**Files:**
- Modify: `crates/alexandria-ffi/Cargo.toml`
- Modify: `crates/alexandria-ffi/tests/parity.rs`

**Interfaces:**
- Consumes: the full extraction pipeline through both surfaces (unchanged public API — this task only adds a new test).

Reuses the exact `local_settings()` / `seed_session()` / `build_services()` / `app()` / `alexandria_index_init` / `alexandria_index_start` / `alexandria_file_get_by_uuid` scaffolding every other test in this file uses. Reuses the same fixture-generation approach as Task 3's unit tests (real MP4 muxed with `ffmpeg-next`). **Both legs must poll on every column the test asserts on before proceeding to the GET/`alexandria_file_get_by_uuid` call** — the image slice's final review found and fixed a residual race where a test polled on one extraction write but asserted on a different, later one; the document slice's Task 8 avoided repeating it; do not repeat it here. This slice writes up to two independent columns per file (`video_files.duration_seconds` in one transaction; `video_files.title`/`resolution` in another), so the wait condition must require every column the assertions check.

- [ ] **Step 1: Add `ffmpeg-next` as an FFI dev-dependency**

The test this task adds builds an MP4 fixture (via `ffmpeg-next`) that carries a title, a known resolution, and enough frames to report a non-zero duration — exercising both independent extraction writes (duration + metadata) in one test.

In `crates/alexandria-ffi/Cargo.toml`'s `[dev-dependencies]` section (currently `alexandria-core`, `alexandria-http`, `axum`, `chrono`, `image`, `little_exif`, `lofty`, `lopdf`, `serde_json`, `sqlx`, `tempfile`, `tokio`, `tower`), add:

```toml
ffmpeg-next.workspace = true
```

(alphabetically after `chrono`, before `image` — chrono < ffmpeg-next < image).

- [ ] **Step 2: Write the test**

Append to the end of `crates/alexandria-ffi/tests/parity.rs`. This mirrors the document slice's `given_tagged_pdf_file_when_indexed_via_http_and_ffi_then_extracted_metadata_matches` test structure closely — read that test first (search for it in this file) to confirm the exact current shape of `local_settings()`, `seed_session()`, `setup_ffi_db()`, and the FFI-leg polling pattern (a `spawn_blocking` closure with its own `tokio::runtime::Runtime` connecting directly to the FFI database file), then write this new test following that same shape with the helpers below substituted in.

```rust
/// Build a minimal valid MP4 with `ffmpeg-next` — mirrors the identical
/// helper in `alexandria-core`'s `catalog::video_tags` unit tests.
fn write_minimal_mp4(path: &std::path::Path, title: &str, width: u32, height: u32) {
    ffmpeg_next::init().expect("ffmpeg init");

    let mut octx = ffmpeg_next::format::output(path).expect("create output context");
    octx.set_metadata({
        let mut dict = ffmpeg_next::Dictionary::new();
        dict.set("title", title);
        dict
    });

    let codec = ffmpeg_next::encoder::find(ffmpeg_next::codec::Id::MPEG4)
        .expect("mpeg4 encoder available");
    let mut ost = octx.add_stream(codec).expect("add video stream");
    let mut encoder = ffmpeg_next::codec::context::Context::new_with_codec(codec)
        .encoder()
        .video()
        .expect("video encoder context");
    encoder.set_width(width);
    encoder.set_height(height);
    encoder.set_format(ffmpeg_next::format::Pixel::YUV420P);
    encoder.set_time_base(ffmpeg_next::Rational(1, 25));
    let mut encoder = encoder.open().expect("open encoder");
    ost.set_parameters(&encoder);

    octx.write_header().expect("write header");

    let mut frame = ffmpeg_next::frame::Video::new(ffmpeg_next::format::Pixel::YUV420P, width, height);
    for plane in 0..frame.planes() {
        frame.data_mut(plane).fill(16);
    }

    for i in 0..10 {
        frame.set_pts(Some(i));
        encoder.send_frame(&frame).expect("send frame");
        let mut packet = ffmpeg_next::Packet::empty();
        while encoder.receive_packet(&mut packet).is_ok() {
            packet.set_stream(0);
            packet.write_interleaved(&mut octx).expect("write packet");
        }
    }
    encoder.send_eof().expect("send eof");
    let mut packet = ffmpeg_next::Packet::empty();
    while encoder.receive_packet(&mut packet).is_ok() {
        packet.set_stream(0);
        packet.write_interleaved(&mut octx).expect("write packet");
    }
    octx.write_trailer().expect("write trailer");
}

/// Poll until `video_files.title`/`video_files.resolution`/
/// `video_files.duration_seconds` are all non-NULL for the named file —
/// proves BOTH extraction writes landed (metadata write and duration
/// write are separate transactions), not just file-row existence or a
/// single write.
async fn wait_for_http_video_extraction(pool: &sqlx::sqlite::SqlitePool, name: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let row: Option<(Option<String>, Option<String>, Option<f64>)> = sqlx::query_as(
            "SELECT video_files.title, video_files.resolution, video_files.duration_seconds \
             FROM video_files \
             JOIN files ON files.id = video_files.file_id \
             WHERE files.name = ?",
        )
        .bind(name)
        .fetch_optional(pool)
        .await
        .unwrap();
        if let Some((Some(_), Some(_), Some(_))) = row {
            return;
        }
        if std::time::Instant::now() > deadline {
            panic!("http never wrote extracted video metadata");
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Issue #44 video slice parity — index a tagged MP4 through both
/// transports and assert the extracted title/resolution/durationSeconds
/// (written by the indexer itself, not by a manual PATCH) are
/// byte-for-byte identical.
#[tokio::test]
async fn given_tagged_mp4_file_when_indexed_via_http_and_ffi_then_extracted_metadata_matches() {
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    let http_video = http_lib.path().join("movie.mp4");
    write_minimal_mp4(&http_video, "Parity Title", 320, 240);

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let index_req = Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "root": http_lib.path().to_str().unwrap() }).to_string(),
        ))
        .unwrap();
    let _ = app(Settings::default(), http_services.clone())
        .oneshot(index_req)
        .await
        .expect("http index");
    wait_for_http_video_extraction(&http_pool, "movie.mp4").await;

    let (http_uuid,): (String,) = sqlx::query_as("SELECT uuid FROM files WHERE name = ?")
        .bind("movie.mp4")
        .fetch_one(&http_pool)
        .await
        .unwrap();

    let get_req = Request::builder()
        .method("GET")
        .uri(format!("/v1/files/{http_uuid}"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let get_resp = app(Settings::default(), http_services)
        .oneshot(get_req)
        .await
        .expect("http get");
    assert_eq!(get_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(get_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_lib = tempdir().unwrap();
    let ffi_video = ffi_lib.path().join("movie.mp4");
    write_minimal_mp4(&ffi_video, "Parity Title", 320, 240);
    let ffi_lib_path = ffi_lib.path().to_str().unwrap().to_string();
    let ffi_db_for_poll = ffi_db.clone();

    let ffi_body: String = tokio::task::spawn_blocking(move || -> String {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let root = CString::new(ffi_lib_path).unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let started = alexandria_index_start(root.as_ptr(), token.as_ptr());
        assert_eq!(started.status, alexandria_ffi::INDEX_OK);

        // Poll the FFI leg's own sqlite file directly for all three
        // extraction writes (title, resolution, duration_seconds) — not
        // just file-row existence, and not just the first of the writes
        // the indexer commits across its separate transactions.
        type FfiVideoExtractionRow = (String, Option<String>, Option<String>, Option<f64>);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ffi_uuid: String = rt.block_on(async {
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect(&format!("sqlite://{ffi_db_for_poll}?mode=rw"))
                .await
                .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let row: Option<FfiVideoExtractionRow> = sqlx::query_as(
                    "SELECT files.uuid, video_files.title, video_files.resolution, \
                     video_files.duration_seconds \
                     FROM video_files \
                     JOIN files ON files.id = video_files.file_id \
                     WHERE files.name = ?",
                )
                .bind("movie.mp4")
                .fetch_optional(&pool)
                .await
                .unwrap();
                if let Some((uuid, Some(_), Some(_), Some(_))) = row {
                    return uuid;
                }
                if std::time::Instant::now() > deadline {
                    panic!("ffi never wrote extracted video metadata");
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        });

        let uuid_c = CString::new(ffi_uuid).unwrap();
        let result = alexandria_file_get_by_uuid(uuid_c.as_ptr(), token.as_ptr());
        assert_eq!(result.status, alexandria_ffi::FILE_OK);
        assert!(!result.json.is_null());
        let json = unsafe { CStr::from_ptr(result.json) }
            .to_str()
            .unwrap()
            .to_string();
        unsafe {
            alexandria_free_string(result.json);
        }
        json
    })
    .await
    .unwrap();

    let ffi_body: serde_json::Value = serde_json::from_str(&ffi_body).unwrap();

    // ---- compare ----
    assert_eq!(http_body["durationSeconds"], ffi_body["durationSeconds"]);
    assert_eq!(http_body["metadata"], ffi_body["metadata"]);
    assert_eq!(http_body["metadata"]["title"], "Parity Title");
    assert_eq!(http_body["metadata"]["resolution"], "320x240");
    let http_duration = http_body["durationSeconds"].as_f64().expect("duration is a number");
    assert!(http_duration > 0.0, "expected a positive duration, got {http_duration}");
}
```

Before finalizing: `FileView` carries `#[serde(rename_all = "camelCase")]` (added during the document slice's final review, applied to the whole struct) — every multi-word field on `FileView`, including this task's new `duration_seconds`, serializes as camelCase (`durationSeconds`). This is different from the document slice's own `page_count`, which shipped as snake_case before that attribute existed and was fixed up afterward — `duration_seconds` gets the correct camelCase form from the start. Before trusting this, read `crates/alexandria-core/src/catalog/model.rs`'s current `FileView` struct definition yourself to confirm the attribute is still present and unchanged, rather than assuming this plan text is still accurate.

- [ ] **Step 3: Run test to verify it passes**

Run: `cargo test -p alexandria-ffi --test parity given_tagged_mp4_file_when_indexed_via_http_and_ffi -- --nocapture`
Expected: `test result: ok. 1 passed`. As with all three prior slices' equivalent task, this only exercises code paths built in Tasks 1–8, so there's no meaningful "write it failing first" step — the assertions either hold given the prior tasks' implementation, or reveal a real bug in it.

- [ ] **Step 4: Run the full parity suite to confirm no regression**

Run: `cargo test -p alexandria-ffi --test parity`
Expected: every test in the file passes.

- [ ] **Step 5: `fmt` + `clippy`**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings` (exact literal commands). Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/alexandria-ffi/Cargo.toml crates/alexandria-ffi/tests/parity.rs
git commit -m "test: add HTTP/FFI parity coverage for extracted video metadata"
```

---

### Task 10: Full verification, PR, and merge

**Files:** none (verification + workflow only)

- [ ] **Step 1: Full workspace verification**

Run: `cargo fmt --all -- --check`
Expected: no diff.

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings.

Run: `cargo test --workspace`
Expected: every test passes across every crate, `0 failed`.

- [ ] **Step 2: Push and open the PR**

```bash
git push -u origin feature/video-metadata-extraction
```

```bash
gh pr create --title "feat: extract video metadata during indexing (issue #44 video slice)" --body "$(cat <<'EOF'
## Summary
- Implements the video slice of issue #44: indexing now reads embedded video container metadata (via `ffmpeg-next`), pre-populating title, year, resolution, and duration instead of leaving every field for the owner to enter manually.
- Format scope: **all 10** video extensions `classify_by_extension` maps to `FileType::Video` (mp4, m4v, mkv, avi, mov, webm, mpg, mpeg, wmv, flv) — unlike prior slices, no extension subset is left unextracted, because `ffmpeg-next` covers the whole set with one reader. This is a deliberate departure from the "pure Rust only" precedent of the audio/image/document slices, and adds this project's first system build dependency (ffmpeg dev libraries) — see the CI workflow change and the design doc for the trade-off discussion.
- `resolution` is a formatted `"WxH"` string written into the existing owner-editable column; `title`/`year` likewise reuse existing columns. `duration_seconds` needed the same narrow new repository method + `FileView` field + migration pattern the image (`width`/`height`) and document (`page_count`) slices established.
- `media_kind` (movie/series) is never auto-set — unlike document's `format_kind`, it isn't inferable from the file itself, and stays owner-only via UC-04.
- Extraction runs once, at first index only; `refresh.rs` is untouched. Extraction failure never fails the indexing run.
- Comic extraction is a separate follow-up slice.

See \`docs/superpowers/specs/2026-08-06-video-metadata-extraction-design.md\` for the full design.

Relates to #44 (does not close it — this is the video slice only).

## Test plan
- [x] \`cargo test --workspace\` — all green
- [x] \`cargo fmt --all\` / \`cargo clippy --workspace --all-targets -- -D warnings\`
- [x] Unit tests: \`FfmpegVideoMetadataReader\` against a real generated MP4 fixture (muxed with \`ffmpeg-next\` itself), repository \`set_video_duration\`/\`find_video_duration\`, \`BrowseFilesHandler::get_by_uuid\` duration wiring, \`IndexHandler\` against \`FakeVideoMetadataReader\` (full tags/duration-only/resolution-only/untagged/non-video, with a call-count assertion proving the reader is never consulted for non-video files)
- [x] HTTP/FFI parity test: index a real tagged MP4 through both surfaces, assert extracted duration + metadata match (race-free — both legs poll on all extraction writes landing, not just file-row existence)
- [x] CI: ffmpeg dev libraries installed as a new system-dependency step, verified green on this PR

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 3: Wait for CI, then merge**

Run: `gh pr checks <PR number> --watch`
Expected: all checks pass. If CI fails specifically on the ffmpeg install step or a subsequent build/link error that looks environment-related rather than code-related, investigate the exact `apt-get`/`pkg-config` error before assuming it's a code defect — this is the first time this project's CI has needed a system dependency, so a packaging/naming mismatch on the runner's Ubuntu version is a more likely first failure mode than a logic bug.

```bash
gh pr merge <PR number> --squash --delete-branch
```

- [ ] **Step 4: Sync `main` and confirm clean tree**

```bash
git switch main
git pull --ff-only
git status --short
```

Expected: no output from `git status --short` (clean tree), `main` at the new merge commit.
