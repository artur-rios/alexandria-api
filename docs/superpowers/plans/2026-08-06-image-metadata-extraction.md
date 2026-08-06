# Image Metadata Extraction (2nd slice of issue #44) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Read embedded EXIF data at index time and pre-populate an image file's pixel dimensions and (when present) title, instead of leaving every field for the owner to type in via UC-04 — and make the dimensions readable back through `GET /v1/files/{uuid}`, since nothing reads them today.

**Architecture:** A new `ImageMetadataReader` trait port (mirroring `AudioMetadataReader` from the audio slice, [PR #80](https://github.com/artur-rios/alexandria-api/pull/80)) becomes a 6th generic collaborator on `IndexHandler`, alongside audio's existing `AudioMetadataReader`. Unlike audio, EXIF's most reliable data — pixel dimensions — lives outside `SubtypeMetadata::Image` entirely (only `title`/`caption` are UC-04-editable), so this slice adds one narrow new repository *write* method (`set_image_dimensions`) and one narrow new repository *read* method (`find_image_dimensions`), plus two new `FileView` fields to carry dimensions back to callers. `title` (when present) still goes through the existing `update_metadata`, exactly like audio.

**Tech Stack:** Rust, `kamadak-exif` (new dependency, real EXIF reader), `image` + `little_exif` (new dev-dependencies, to generate a real tagged JPEG fixture at test time — `kamadak-exif` is read-only, so unlike audio's `lofty` it can't both read and write the test fixture itself).

## Global Constraints

- Spec docs: `docs/superpowers/specs/2026-08-06-image-metadata-extraction-design.md` (read this first if anything below is ambiguous) and its amendment covering the width/height read path (same file, "New repository read method and `FileView` field addition" section).
- Extraction runs **once, at first index only**. Never touch `refresh.rs`.
- Extraction failure (no EXIF, corrupt file, unsupported format) is **never** a run failure: not counted in `IndexOutcome::failed`, logged at `debug` at most.
- The dimensions write (`set_image_dimensions`) and the title write (`update_metadata`) are **independent** — a failure in one must not block or be conflated with the other, and neither fails indexing.
- `caption` is never touched by extraction — stays owner-only, exactly as before this slice.
- `kamadak-exif`, `little_exif`, and `image` crate exact method/type names are best-effort based on their documented APIs at the time of writing; if a name has moved in the resolved version, fix it against `cargo doc -p <crate> --open` — this is the same situation the audio slice's Task 2 handled successfully with `lofty`, and the same approach applies here.
- Every new/changed Rust file must pass `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` before its task is done — report the **exact literal commands**, not narrower or `--check`-only variants (this was flagged repeatedly during the audio slice's review; get it right the first time).
- Branch: `feature/image-metadata-extraction` off `main`. One PR at the end of Task 9, following this repo's established branch → PR → CI → squash-merge cycle.

---

### Task 1: `ImageTags` type and `ImageMetadataReader` trait

**Files:**
- Create: `crates/alexandria-core/src/catalog/image_tags.rs`
- Modify: `crates/alexandria-core/src/catalog/mod.rs`

**Interfaces:**
- Produces: `pub struct ImageTags { pub width: Option<i64>, pub height: Option<i64>, pub title: Option<String> }`
- Produces: `#[allow(async_fn_in_trait)] pub trait ImageMetadataReader: Send + Sync { async fn read(&self, path: &str) -> Option<ImageTags>; }`

Pure logic, no I/O, no new dependency yet — mirrors the audio slice's Task 1 exactly in shape.

- [ ] **Step 1: Write the file**

Create `crates/alexandria-core/src/catalog/image_tags.rs`:

```rust
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
```

Add the module to `crates/alexandria-core/src/catalog/mod.rs` — it currently reads:

```rust
pub mod audio_tags;
pub mod classify;
pub mod clock;
pub mod commands;
pub mod fs;
pub mod model;
pub mod queries;
pub mod repos;
```

Change to (alphabetical):

```rust
pub mod audio_tags;
pub mod classify;
pub mod clock;
pub mod commands;
pub mod fs;
pub mod image_tags;
pub mod model;
pub mod queries;
pub mod repos;
```

- [ ] **Step 2: Confirm it compiles**

Run: `cargo build -p alexandria-core`
Expected: builds cleanly (no tests yet — this file has no logic branch to unit-test on its own, unlike `AudioTags::into_subtype_metadata`; `ImageTags` has no analogous conversion method since its two write paths, `set_image_dimensions` and `update_metadata`, are called directly by `IndexHandler` in Task 6, not through a single combinator).

- [ ] **Step 3: `fmt` + `clippy`**

Run: `cargo fmt --all && cargo clippy -p alexandria-core --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 4: Commit**

```bash
git add crates/alexandria-core/src/catalog/image_tags.rs crates/alexandria-core/src/catalog/mod.rs
git commit -m "feat: add ImageTags and ImageMetadataReader port"
```

---

### Task 2: `ExifImageMetadataReader` (real implementation)

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Modify: `crates/alexandria-core/Cargo.toml`
- Modify: `crates/alexandria-core/src/catalog/image_tags.rs`

**Interfaces:**
- Consumes: `ImageTags`, `ImageMetadataReader` from Task 1.
- Produces: `#[derive(Debug, Default, Clone, Copy)] pub struct ExifImageMetadataReader;` implementing `ImageMetadataReader`.

This is the riskiest task in the plan: `kamadak-exif` is read-only, so unlike the audio slice (where `lofty` could both read and write its own test fixture), this task needs a *separate* way to produce a real, tagged JPEG to test against. The approach: use the `image` crate (dev-only) to encode a tiny real JPEG, then `little_exif` (dev-only) to write EXIF tags into it, then read it back with the production `ExifImageMetadataReader`.

- [ ] **Step 1: Add the dependencies**

In `Cargo.toml` (workspace root), `[workspace.dependencies]` — insert alphabetically:

```toml
argon2 = { version = "0.5", features = ["std"] }
axum = "0.8"
cbindgen = "0.29"
chrono = { version = "0.4", features = ["serde"] }
jsonwebtoken = "9"
kamadak-exif = "0.5"
lofty = "0.22"
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
```

(`kamadak-exif` goes between `jsonwebtoken` and `lofty`.)

In the same file's dev-only section (currently just `tempfile = "3"`), add two dev-only dependencies:

```toml
# dev-only
image = { version = "0.25", default-features = false, features = ["jpeg"] }
little_exif = "0.6"
tempfile = "3"
```

In `crates/alexandria-core/Cargo.toml`'s `[dependencies]` — insert alphabetically after `jsonwebtoken`:

```toml
jsonwebtoken.workspace = true
kamadak-exif.workspace = true
lofty.workspace = true
```

In `crates/alexandria-core/Cargo.toml`'s `[dev-dependencies]` (currently `tempfile`, `tokio`, `toml`) — add:

```toml
[dev-dependencies]
image.workspace = true
little_exif.workspace = true
tempfile.workspace = true
tokio.workspace = true
toml.workspace = true
```

Run: `cargo build -p alexandria-core --all-targets`
Expected: builds successfully, `Cargo.lock` updates. If any of the three crate names, versions, or feature flags don't resolve as written, adjust the version to the latest available for that major line and note the change in your report — this is a normal dependency-resolution step, not a plan defect.

- [ ] **Step 2: Write the failing test**

Append to `crates/alexandria-core/src/catalog/image_tags.rs`, inside a new `#[cfg(test)] mod tests` block at the end of the file:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a tiny real JPEG (4x3 pixels, arbitrary solid color) using the
    /// `image` crate — a real, valid JPEG file, not hand-crafted bytes.
    fn write_minimal_jpeg(path: &std::path::Path) {
        let img = image::RgbImage::from_pixel(4, 3, image::Rgb([128, 64, 32]));
        img.save(path).expect("encode jpeg");
    }

    /// Write EXIF tags (pixel dimensions + an ImageDescription) into an
    /// existing JPEG using `little_exif`.
    fn write_test_exif(path: &std::path::Path, width: u32, height: u32, description: &str) {
        use little_exif::exif_tag::ExifTag;
        use little_exif::metadata::Metadata;

        let mut metadata = Metadata::new();
        metadata.set_tag(ExifTag::ImageDescription(description.to_string()));
        metadata.set_tag(ExifTag::PixelXDimension(vec![width]));
        metadata.set_tag(ExifTag::PixelYDimension(vec![height]));
        metadata.write_to_file(path).expect("write exif");
    }

    #[tokio::test]
    async fn given_tagged_jpeg_when_read_then_dimensions_and_title_extracted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tagged.jpg");
        write_minimal_jpeg(&path);
        write_test_exif(&path, 64, 48, "Test Description");

        let reader = ExifImageMetadataReader;
        let tags = reader
            .read(path.to_str().unwrap())
            .await
            .expect("tags extracted");

        assert_eq!(tags.width, Some(64));
        assert_eq!(tags.height, Some(48));
        assert_eq!(tags.title.as_deref(), Some("Test Description"));
    }

    #[tokio::test]
    async fn given_untagged_jpeg_when_read_then_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("untagged.jpg");
        write_minimal_jpeg(&path);

        let reader = ExifImageMetadataReader;
        let tags = reader.read(path.to_str().unwrap()).await;

        assert!(tags.is_none(), "no EXIF written, no EXIF read");
    }

    #[tokio::test]
    async fn given_missing_file_when_read_then_none_not_panic() {
        let reader = ExifImageMetadataReader;

        let tags = reader.read("/no/such/file.jpg").await;

        assert!(tags.is_none());
    }
}
```

If `little_exif`'s `ExifTag` variant names or `Metadata` API differ from the above (e.g. `write_to_file` takes different argument types, or the variant is `PixelXDimension(u32)` instead of `Vec<u32>`), adapt to the real API via `cargo doc -p little_exif --open` — keep the same intent (write pixel dimensions + a description string onto a real JPEG file).

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p alexandria-core --lib catalog::image_tags`
Expected: fails to compile — `ExifImageMetadataReader` does not exist yet.

- [ ] **Step 4: Implement `ExifImageMetadataReader`**

Add above the `#[cfg(test)]` block in `image_tags.rs`:

```rust
/// Real image-EXIF reader backed by `kamadak-exif`, covering JPEG, TIFF,
/// HEIC, and PNG's `eXIf` chunk — 4 of the 9 extensions
/// `classify_by_extension` maps to `FileType::Image` (jpg/jpeg/tif/tiff, and
/// PNG when it carries an `eXIf` chunk). gif/webp/bmp/svg have no EXIF to
/// extract and always yield no metadata — the same graceful degradation the
/// audio slice established for `.wma`.
#[derive(Debug, Default, Clone, Copy)]
pub struct ExifImageMetadataReader;

impl ImageMetadataReader for ExifImageMetadataReader {
    async fn read(&self, path: &str) -> Option<ImageTags> {
        let file = std::fs::File::open(path).ok()?;
        let mut bufreader = std::io::BufReader::new(&file);
        let exif = match exif::Reader::new().read_from_container(&mut bufreader) {
            Ok(e) => e,
            Err(err) => {
                tracing::debug!(path, error = %err, "could not parse image EXIF data");
                return None;
            }
        };

        let width = exif
            .get_field(exif::Tag::PixelXDimension, exif::In::PRIMARY)
            .or_else(|| exif.get_field(exif::Tag::ImageWidth, exif::In::PRIMARY))
            .and_then(|f| f.value.get_uint(0))
            .map(i64::from);
        let height = exif
            .get_field(exif::Tag::PixelYDimension, exif::In::PRIMARY)
            .or_else(|| exif.get_field(exif::Tag::ImageLength, exif::In::PRIMARY))
            .and_then(|f| f.value.get_uint(0))
            .map(i64::from);
        let title = exif
            .get_field(exif::Tag::ImageDescription, exif::In::PRIMARY)
            .and_then(|f| match &f.value {
                exif::Value::Ascii(vecs) => vecs.first().map(|b| String::from_utf8_lossy(b).into_owned()),
                _ => None,
            })
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        if width.is_none() && height.is_none() && title.is_none() {
            return None;
        }

        Some(ImageTags {
            width,
            height,
            title,
        })
    }
}
```

If `exif::Reader`, `exif::Tag`, `exif::In`, or `exif::Value` don't match the resolved `kamadak-exif` version's actual API, adapt via `cargo doc -p kamadak-exif --open` (the crate's Rust module name is `exif`, not `kamadak_exif` — `use exif::...` is correct even though the Cargo.toml dependency line says `kamadak-exif`). Keep the same intent: try the Exif SubIFD dimension tags first, fall back to the IFD0 tags, extract `ImageDescription` as a trimmed, non-empty string only.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p alexandria-core --lib catalog::image_tags`
Expected: `test result: ok. 3 passed; 0 failed`

- [ ] **Step 6: `fmt` + `clippy`**

Run: `cargo fmt --all && cargo clippy -p alexandria-core --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock crates/alexandria-core/Cargo.toml crates/alexandria-core/src/catalog/image_tags.rs
git commit -m "feat: implement ExifImageMetadataReader"
```

---

### Task 3: `FakeImageMetadataReader` test double

**Files:**
- Modify: `crates/alexandria-core/tests/common/mod.rs`

**Interfaces:**
- Consumes: `ImageMetadataReader`, `ImageTags` from `alexandria_core::catalog::image_tags`.
- Produces: `FakeImageMetadataReader::new()`, `.seed(path: &str, tags: ImageTags)`, `.call_count()`, implementing `ImageMetadataReader`.

Mirrors `FakeAudioMetadataReader` (already in this file) exactly, including the call-count pattern that proves the reader is never consulted for non-image files.

- [ ] **Step 1: Add the fake**

Add this import near the top of `crates/alexandria-core/tests/common/mod.rs`, alongside the existing `alexandria_core::catalog::audio_tags::...` import:

```rust
use alexandria_core::catalog::audio_tags::{AudioMetadataReader, AudioTags};
use alexandria_core::catalog::image_tags::{ImageMetadataReader, ImageTags};
```

Append this new fake at the end of the file, after `FakeAudioMetadataReader`'s `impl AudioMetadataReader for FakeAudioMetadataReader` block:

```rust
/// In-memory image-EXIF reader (issue #44 image slice). `read()` answers
/// `None` for any path with no seeded tags, mirroring "no EXIF found /
/// couldn't parse" — the same outcome `ExifImageMetadataReader` produces
/// for those cases. Also counts calls, so a test can assert the reader was
/// never consulted at all (e.g. for a non-image file).
#[derive(Debug, Default, Clone)]
pub struct FakeImageMetadataReader {
    tags: Arc<Mutex<HashMap<String, ImageTags>>>,
    call_count: Arc<Mutex<usize>>,
}

impl FakeImageMetadataReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the tags `read()` returns for `path`.
    pub fn seed(&self, path: &str, tags: ImageTags) -> &Self {
        self.tags.lock().unwrap().insert(path.to_string(), tags);
        self
    }

    /// How many times `read()` has been called.
    pub fn call_count(&self) -> usize {
        *self.call_count.lock().unwrap()
    }
}

impl ImageMetadataReader for FakeImageMetadataReader {
    async fn read(&self, path: &str) -> Option<ImageTags> {
        *self.call_count.lock().unwrap() += 1;
        self.tags.lock().unwrap().get(path).cloned()
    }
}
```

- [ ] **Step 2: Confirm it compiles**

Run: `cargo test -p alexandria-core --test catalog -- --list`
Expected: compiles cleanly (the fake being unused so far is fine — `common/mod.rs` already has a module-level `#![allow(dead_code)]`).

- [ ] **Step 3: `fmt` + `clippy`**

Run: `cargo fmt --all && cargo clippy -p alexandria-core --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 4: Commit**

```bash
git add crates/alexandria-core/tests/common/mod.rs
git commit -m "test: add FakeImageMetadataReader test double"
```

---

### Task 4: Repository methods `set_image_dimensions` and `find_image_dimensions`

**Files:**
- Modify: `crates/alexandria-core/src/catalog/repos.rs`
- Modify: `crates/alexandria-core/tests/common/mod.rs`

**Interfaces:**
- Produces (on `CatalogRepository` trait and its `SqliteCatalogRepository`/`FakeCatalogRepository` implementations):
  - `async fn set_image_dimensions(&self, uuid: Uuid, width: i64, height: i64) -> Result<(), DomainError>`
  - `async fn find_image_dimensions(&self, uuid: Uuid) -> Result<Option<(i64, i64)>, DomainError>`
- Produces on `FakeCatalogRepository`: `pub fn dimensions_for(&self, uuid: Uuid) -> Option<(i64, i64)>` (test inspector, mirrors the existing `metadata_for`).

This is real database logic — TDD it against the fake first (fast, no real DB), then implement the real Sqlite version, then add an integration-level check in a later task (Task 8's parity test exercises the real Sqlite path end to end; there's no existing "repository against real Sqlite" unit test file in this codebase to add a narrower one to — the parity test is this codebase's established way of proving Sqlite SQL is correct).

- [ ] **Step 1: Add the trait methods**

In `crates/alexandria-core/src/catalog/repos.rs`, in the `CatalogRepository` trait, add these two methods right after the existing `find_metadata_by_uuid` method (which currently ends around line 68 with `) -> Result<Option<SubtypeMetadata>, DomainError>;`):

```rust
    /// Write an image file's pixel dimensions (issue #44 image slice).
    /// Unlike `update_metadata`, this touches `images.width`/`images.height`
    /// directly — columns `SubtypeMetadata::Image` deliberately excludes
    /// because they are not owner-editable (UC-04). Returns `NotFound` when
    /// no file row carries the UUID, `InvalidInput` when the file is not an
    /// image.
    async fn set_image_dimensions(
        &self,
        uuid: Uuid,
        width: i64,
        height: i64,
    ) -> Result<(), DomainError>;

    /// Read an image file's pixel dimensions, if both are set (issue #44
    /// image slice). `None` when the file doesn't exist, isn't an image, or
    /// either column is still `NULL` (extraction never ran, or found no
    /// dimensions).
    async fn find_image_dimensions(&self, uuid: Uuid) -> Result<Option<(i64, i64)>, DomainError>;
```

- [ ] **Step 2: Add the fakes (write the failing test consumers first)**

In `crates/alexandria-core/tests/common/mod.rs`, add a new field to `FakeCatalogRepository`'s struct definition — it currently ends with:

```rust
    /// File uuid -> collection uuid, as written by `set_collection` (UC-13).
    collection_links: Arc<Mutex<HashMap<Uuid, Uuid>>>,
}
```

Change to:

```rust
    /// File uuid -> collection uuid, as written by `set_collection` (UC-13).
    collection_links: Arc<Mutex<HashMap<Uuid, Uuid>>>,
    /// File uuid -> (width, height), as written by `set_image_dimensions`
    /// (issue #44 image slice).
    dimensions: Arc<Mutex<HashMap<Uuid, (i64, i64)>>>,
}
```

Add an inspector method in `impl FakeCatalogRepository` block, right after the existing `collection_for_file` method:

```rust
    /// Dimensions last written for `uuid` via `set_image_dimensions`. `None`
    /// means no call has landed for that file yet.
    pub fn dimensions_for(&self, uuid: Uuid) -> Option<(i64, i64)> {
        self.dimensions.lock().unwrap().get(&uuid).copied()
    }
```

Add the two trait method implementations in `impl CatalogRepository for FakeCatalogRepository`, right after the existing `find_metadata_by_uuid` implementation:

```rust
    async fn set_image_dimensions(
        &self,
        uuid: Uuid,
        width: i64,
        height: i64,
    ) -> Result<(), DomainError> {
        let files = self.files.lock().unwrap();
        let file = files.values().find(|f| f.uuid == uuid).ok_or(DomainError::NotFound)?;
        if file.file_type != alexandria_core::catalog::model::FileType::Image {
            return Err(DomainError::InvalidInput("file is not an image".into()));
        }
        drop(files);
        self.dimensions.lock().unwrap().insert(uuid, (width, height));
        Ok(())
    }

    async fn find_image_dimensions(&self, uuid: Uuid) -> Result<Option<(i64, i64)>, DomainError> {
        Ok(self.dimensions.lock().unwrap().get(&uuid).copied())
    }
```

- [ ] **Step 3: Confirm the fakes compile**

Run: `cargo test -p alexandria-core --test catalog -- --list`
Expected: compiles cleanly. `cargo build -p alexandria-core` will still fail at this point — the trait now has two new required methods and `SqliteCatalogRepository` doesn't implement them yet. That's expected; the next step fixes it.

- [ ] **Step 4: Implement the real Sqlite methods**

In `crates/alexandria-core/src/catalog/repos.rs`, in `impl CatalogRepository for SqliteCatalogRepository`, add these two methods right after the existing `find_metadata_by_uuid` implementation (which ends around line 610, just before `async fn rename_file`):

```rust
    async fn set_image_dimensions(
        &self,
        uuid: Uuid,
        width: i64,
        height: i64,
    ) -> Result<(), DomainError> {
        let mut tx = self.pool.begin().await?;

        let (id, type_str): (i64, String) =
            sqlx::query_as("SELECT id, type FROM files WHERE uuid = ?")
                .bind(uuid.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(DomainError::NotFound)?;

        let actual_type = parse_type_str(&type_str)?;
        if actual_type != FileType::Image {
            return Err(DomainError::InvalidInput("file is not an image".into()));
        }

        let affected = sqlx::query("UPDATE images SET width = ?, height = ? WHERE file_id = ?")
            .bind(width)
            .bind(height)
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();

        if affected == 0 {
            return Err(DomainError::internal(format!(
                "subtype row missing for file {uuid} (image)"
            )));
        }

        tx.commit().await?;
        Ok(())
    }

    async fn find_image_dimensions(&self, uuid: Uuid) -> Result<Option<(i64, i64)>, DomainError> {
        let row: Option<(i64, String)> =
            sqlx::query_as("SELECT id, type FROM files WHERE uuid = ?")
                .bind(uuid.to_string())
                .fetch_optional(&self.pool)
                .await?;
        let (id, type_str) = match row {
            Some(r) => r,
            None => return Ok(None),
        };
        if parse_type_str(&type_str)? != FileType::Image {
            return Ok(None);
        }

        let dims: Option<(Option<i64>, Option<i64>)> =
            sqlx::query_as("SELECT width, height FROM images WHERE file_id = ?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;

        Ok(dims.and_then(|(w, h)| match (w, h) {
            (Some(w), Some(h)) => Some((w, h)),
            _ => None,
        }))
    }
```

- [ ] **Step 5: Verify the fakes and the workspace build together**

Run: `cargo build --workspace`
Expected: builds cleanly — this is the point where `SqliteCatalogRepository` finally satisfies the widened trait, so anything that was broken by Step 1 alone is now fixed.

Run: `cargo test -p alexandria-core --test catalog`
Expected: all existing catalog tests still pass (nothing in this task changed behavior for any existing method).

- [ ] **Step 6: `fmt` + `clippy`**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/alexandria-core/src/catalog/repos.rs crates/alexandria-core/tests/common/mod.rs
git commit -m "feat: add set_image_dimensions and find_image_dimensions to CatalogRepository"
```

---

### Task 5: `FileView` width/height and `BrowseFilesHandler::get_by_uuid` wiring

**Files:**
- Modify: `crates/alexandria-core/src/catalog/model.rs`
- Modify: `crates/alexandria-core/src/catalog/queries/browse.rs`
- Test: `crates/alexandria-core/tests/catalog/browse.rs`

**Interfaces:**
- Consumes: `CatalogRepository::find_image_dimensions` from Task 4.
- Produces: `FileView { file: File, metadata: Option<SubtypeMetadata>, width: Option<i64>, height: Option<i64> }` (was `{ file, metadata }`).

This closes the read-path gap: without this task, `set_image_dimensions` writes data nothing can ever read back. No HTTP or FFI code needs to change — both already serialize `FileView` generically (`crates/alexandria-http/src/routes/browse.rs`'s `get_file` returns `Json<FileView>` directly; `crates/alexandria-ffi/src/lib.rs`'s `alexandria_file_get_by_uuid` calls `serde_json::to_string` on the same `FileView`).

- [ ] **Step 1: Write the failing test**

This file already has a `handler(auth, repo) -> BrowseFilesHandler<A, R>` helper and a `TOKEN` constant (`const TOKEN: &str = "bearer-token";`), and its existing `get_by_uuid` tests follow this exact pattern (see e.g. `given_existing_file_when_get_by_uuid_then_file_view_returned`, around line 218): construct a `File` via `existing_file_with_hash(path, name, file_type, hash)`, insert it into a fresh `FakeCatalogRepository` via `repo.seed(file)` (not `with_existing` — that's a different helper used elsewhere), then build the handler and call `get_by_uuid`. Add these 3 new tests near the existing `get_by_uuid` tests, following that same pattern exactly:

```rust
#[tokio::test]
async fn given_image_with_extracted_dimensions_when_get_by_uuid_then_width_and_height_present() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/photo.jpg", "photo", FileType::Image, "h");
    let uuid = file.uuid;
    repo.seed(file);
    repo.set_image_dimensions(uuid, 800, 600).await.unwrap();

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    assert_eq!(view.width, Some(800));
    assert_eq!(view.height, Some(600));
}

#[tokio::test]
async fn given_image_with_no_extracted_dimensions_when_get_by_uuid_then_width_and_height_none() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/photo.jpg", "photo", FileType::Image, "h");
    let uuid = file.uuid;
    repo.seed(file);

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    assert_eq!(view.width, None);
    assert_eq!(view.height, None);
}

#[tokio::test]
async fn given_non_image_file_when_get_by_uuid_then_width_and_height_none() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/song.mp3", "song", FileType::Audio, "h");
    let uuid = file.uuid;
    repo.seed(file);

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    assert_eq!(view.width, None);
    assert_eq!(view.height, None);
}
```

`existing_file_with_hash`, `FakeCatalogRepository`, `FakeAuth`, `FileType`, `SubtypeMetadata` are all already imported at the top of this file (used by the existing tests) — no new imports needed for these 3 tests. `set_image_dimensions` is the new `CatalogRepository` trait method from Task 4, already in scope via the existing `use alexandria_core::catalog::repos::CatalogRepository;` import this file already has (needed since `FakeCatalogRepository` implements that trait).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p alexandria-core --test catalog -- browse::`
Expected: fails to compile — `FileView` has no `width`/`height` fields yet, and `get_by_uuid`'s return doesn't set them.

- [ ] **Step 3: Add the fields to `FileView`**

In `crates/alexandria-core/src/catalog/model.rs`, change:

```rust
#[derive(Debug, Clone, Serialize)]
pub struct FileView {
    pub file: File,
    /// `None` when the subtype has no editable metadata (Text/Html), or when
    /// no metadata has been written to the subtype row yet.
    pub metadata: Option<SubtypeMetadata>,
}
```

to:

```rust
#[derive(Debug, Clone, Serialize)]
pub struct FileView {
    pub file: File,
    /// `None` when the subtype has no editable metadata (Text/Html), or when
    /// no metadata has been written to the subtype row yet.
    pub metadata: Option<SubtypeMetadata>,
    /// Extracted pixel dimensions (issue #44 image slice). `None` for every
    /// non-image file, and for an image file whose dimensions haven't been
    /// extracted yet.
    pub width: Option<i64>,
    pub height: Option<i64>,
}
```

- [ ] **Step 4: Wire the read in `BrowseFilesHandler::get_by_uuid`**

In `crates/alexandria-core/src/catalog/queries/browse.rs`, change:

```rust
    pub async fn get_by_uuid(&self, uuid: Uuid, token: &str) -> Result<FileView, DomainError> {
        // AF-02: the caller must be authenticated.
        self.auth.authenticate(token).await?;

        // AF-01: the file must exist.
        let file = self
            .repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        let metadata = self.repo.find_metadata_by_uuid(uuid).await?;

        Ok(FileView { file, metadata })
    }
```

to:

```rust
    pub async fn get_by_uuid(&self, uuid: Uuid, token: &str) -> Result<FileView, DomainError> {
        // AF-02: the caller must be authenticated.
        self.auth.authenticate(token).await?;

        // AF-01: the file must exist.
        let file = self
            .repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        let metadata = self.repo.find_metadata_by_uuid(uuid).await?;

        // Issue #44 image slice: width/height live outside `SubtypeMetadata`
        // (see `find_image_dimensions`'s doc comment), so they're fetched
        // separately and only for image files.
        let (width, height) = if file.file_type == FileType::Image {
            match self.repo.find_image_dimensions(uuid).await? {
                Some((w, h)) => (Some(w), Some(h)),
                None => (None, None),
            }
        } else {
            (None, None)
        };

        Ok(FileView {
            file,
            metadata,
            width,
            height,
        })
    }
```

`FileType` is already imported in this file (`use crate::catalog::model::{File, FileType, FileView, StateFilter};`), so no new import is needed.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p alexandria-core --test catalog -- browse::`
Expected: all pass, including the 3 new ones.

- [ ] **Step 6: Run the full alexandria-core test suite**

Run: `cargo test -p alexandria-core`
Expected: all pass — this confirms no other existing test constructs a `FileView` literal that the new required fields would break (none currently do, per a workspace-wide grep, but re-confirm here).

- [ ] **Step 7: `fmt` + `clippy`**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/alexandria-core/src/catalog/model.rs crates/alexandria-core/src/catalog/queries/browse.rs crates/alexandria-core/tests/catalog/browse.rs
git commit -m "feat: surface extracted image dimensions through FileView"
```

---

### Task 6: Wire `ImageMetadataReader` into `IndexHandler`

**Files:**
- Modify: `crates/alexandria-core/src/catalog/commands/index.rs`
- Modify: `crates/alexandria-core/tests/catalog/index.rs`

**Interfaces:**
- Consumes: `ImageMetadataReader`, `ImageTags` from Task 1; `FakeImageMetadataReader` from Task 3; `CatalogRepository::set_image_dimensions` from Task 4.
- Produces: `IndexHandler<A, R, F, C, M, N>` (was `<A, R, F, C, M>`) — `N: ImageMetadataReader` is the new 6th parameter, with `pub fn new(auth: A, repo: R, fs: F, clock: C, audio_tags: M, image_tags: N) -> Self` (was 5 params).

Mirrors the audio slice's Task 4 exactly in shape: widening `IndexHandler`'s constructor arity means every existing call site needs the new argument — this file's own tests, plus (in Task 7) `services.rs`. As with the audio slice, this task deliberately leaves `services.rs` broken; fixing it is Task 7's job, not this one's.

- [ ] **Step 1: Write the failing tests**

In `crates/alexandria-core/tests/catalog/index.rs`, make these edits.

**1a.** Add to the imports block (it currently imports `AudioMetadataReader`/`AudioTags` and `FakeAudioMetadataReader`):

```rust
use alexandria_core::catalog::audio_tags::{AudioMetadataReader, AudioTags};
use alexandria_core::catalog::image_tags::{ImageMetadataReader, ImageTags};
```

and in the `use crate::common::{...}` import list, add `FakeImageMetadataReader` alongside the existing `FakeAudioMetadataReader`.

**1b.** Change the `handler` helper function. It currently reads:

```rust
fn handler<A, R, F, C, M>(
    auth: A,
    repo: R,
    fs: F,
    clock: C,
    audio_tags: M,
) -> IndexHandler<A, R, F, C, M>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
    C: Clock,
    M: AudioMetadataReader,
{
    IndexHandler::new(auth, repo, fs, clock, audio_tags)
}
```

Change to:

```rust
fn handler<A, R, F, C, M, N>(
    auth: A,
    repo: R,
    fs: F,
    clock: C,
    audio_tags: M,
    image_tags: N,
) -> IndexHandler<A, R, F, C, M, N>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
    C: Clock,
    M: AudioMetadataReader,
    N: ImageMetadataReader,
{
    IndexHandler::new(auth, repo, fs, clock, audio_tags, image_tags)
}
```

**1c.** Every existing call to `handler(...)` in this file (8 call sites from the audio slice, all already passing 5 arguments) needs a 6th argument, `FakeImageMetadataReader::new()`. As in the audio slice's Task 4, these fall into a small number of distinct literal shapes — find every occurrence of `handler(` in the file:

Run: `grep -n "handler(" crates/alexandria-core/tests/catalog/index.rs`

For each multi-line call ending in `);` whose last argument is currently `FakeAudioMetadataReader::new()` (either as the sole trailing argument on its own line, or inline as `..., FakeAudioMetadataReader::new())`), add `, FakeImageMetadataReader::new()` immediately after it, before the closing `)`. Every one of these calls gets exactly this same change — a single find-all-replace of `FakeAudioMetadataReader::new())` → `FakeAudioMetadataReader::new(), FakeImageMetadataReader::new())` covers every occurrence, since that exact substring is unique to these call sites' closing arguments.

After the replacement, confirm none were missed:

Run: `grep -c "FakeImageMetadataReader::new()" crates/alexandria-core/tests/catalog/index.rs`
Expected: the same count as `grep -c "FakeAudioMetadataReader::new()" crates/alexandria-core/tests/catalog/index.rs` (both should match the number of `handler(...)` call sites in the file — 8, plus the 3 new audio-tag tests added in the audio slice, so check both counts agree with each other rather than assuming a specific number, since this file has grown since the audio slice's Task 4 wrote it).

**1d.** Add these new tests at the end of the file (after the last existing test):

```rust
#[tokio::test]
async fn given_tagged_image_file_when_execute_then_dimensions_and_title_written() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.jpg", "a.jpg", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let image_tags = FakeImageMetadataReader::new();
    image_tags.seed(
        "/library/a.jpg",
        ImageTags {
            width: Some(800),
            height: Some(600),
            title: Some("A Photo".to_string()),
        },
    );
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        image_tags,
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.jpg").expect("indexed");
    assert_eq!(repo_handle.dimensions_for(a.uuid), Some((800, 600)));
    let metadata = repo_handle
        .metadata_for(a.uuid)
        .expect("title written from extracted tags");
    assert_eq!(
        metadata,
        alexandria_core::catalog::model::SubtypeMetadata::Image {
            title: Some("A Photo".to_string()),
            caption: None,
        }
    );
}

#[tokio::test]
async fn given_image_with_dimensions_but_no_title_when_execute_then_only_dimensions_written() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.jpg", "a.jpg", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let image_tags = FakeImageMetadataReader::new();
    image_tags.seed(
        "/library/a.jpg",
        ImageTags {
            width: Some(800),
            height: Some(600),
            title: None,
        },
    );
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        image_tags,
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.jpg").expect("indexed");
    assert_eq!(repo_handle.dimensions_for(a.uuid), Some((800, 600)));
    assert!(
        repo_handle.metadata_for(a.uuid).is_none(),
        "no title extracted means update_metadata is never called"
    );
}

#[tokio::test]
async fn given_untagged_image_file_when_execute_then_neither_write_happens() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.jpg", "a.jpg", "h-a")
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
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.jpg").expect("indexed");
    assert_eq!(repo_handle.dimensions_for(a.uuid), None);
    assert!(repo_handle.metadata_for(a.uuid).is_none());
}

#[tokio::test]
async fn given_non_image_file_when_execute_then_image_reader_never_consulted() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/notes.md", "notes.md", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let audio_tags = FakeAudioMetadataReader::new();
    let image_tags = FakeImageMetadataReader::new();
    let image_tags_handle = image_tags.clone();
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        audio_tags,
        image_tags,
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    assert_eq!(
        image_tags_handle.call_count(),
        0,
        "the image reader must not be consulted at all for a non-image file"
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p alexandria-core --test catalog -- index::`
Expected: fails to compile — constructor arity mismatch, `image_tags` field doesn't exist on `IndexHandler` yet.

- [ ] **Step 3: Implement the change in `index.rs`**

In `crates/alexandria-core/src/catalog/commands/index.rs`, make these exact edits.

Add to the imports:

```rust
use crate::catalog::audio_tags::AudioMetadataReader;
use crate::catalog::image_tags::ImageMetadataReader;
```

Change the struct + constructor from:

```rust
pub struct IndexHandler<A, R, F, C, M> {
    auth: A,
    repo: R,
    fs: F,
    clock: C,
    audio_tags: M,
}

impl<A, R, F, C, M> IndexHandler<A, R, F, C, M>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
    C: Clock,
    M: AudioMetadataReader,
{
    pub fn new(auth: A, repo: R, fs: F, clock: C, audio_tags: M) -> Self {
        Self {
            auth,
            repo,
            fs,
            clock,
            audio_tags,
        }
    }
```

to:

```rust
pub struct IndexHandler<A, R, F, C, M, N> {
    auth: A,
    repo: R,
    fs: F,
    clock: C,
    audio_tags: M,
    image_tags: N,
}

impl<A, R, F, C, M, N> IndexHandler<A, R, F, C, M, N>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
    C: Clock,
    M: AudioMetadataReader,
    N: ImageMetadataReader,
{
    pub fn new(auth: A, repo: R, fs: F, clock: C, audio_tags: M, image_tags: N) -> Self {
        Self {
            auth,
            repo,
            fs,
            clock,
            audio_tags,
            image_tags,
        }
    }
```

Change `index_entry`'s existing audio block from:

```rust
        // Best-effort audio tag prefill (issue #44 pilot). Extraction only
        // ever runs here, at first index — refresh never touches metadata.
        // A parse failure or a write failure here must not fail indexing
        // (it is not counted in `IndexOutcome::failed`).
        if file_type == FileType::Audio {
            if let Some(metadata) = self
                .audio_tags
                .read(&entry.path)
                .await
                .and_then(|tags| tags.into_subtype_metadata())
            {
                if let Err(err) = self.repo.update_metadata(file.uuid, &metadata).await {
                    tracing::warn!(
                        path = %entry.path,
                        error = %err,
                        "indexed but failed to write extracted audio tags"
                    );
                }
            }
        }
        Ok(true)
    }
```

to:

```rust
        // Best-effort audio tag prefill (issue #44 pilot). Extraction only
        // ever runs here, at first index — refresh never touches metadata.
        // A parse failure or a write failure here must not fail indexing
        // (it is not counted in `IndexOutcome::failed`).
        if file_type == FileType::Audio {
            if let Some(metadata) = self
                .audio_tags
                .read(&entry.path)
                .await
                .and_then(|tags| tags.into_subtype_metadata())
            {
                if let Err(err) = self.repo.update_metadata(file.uuid, &metadata).await {
                    tracing::warn!(
                        path = %entry.path,
                        error = %err,
                        "indexed but failed to write extracted audio tags"
                    );
                }
            }
        }

        // Best-effort image EXIF prefill (issue #44 image slice). Two
        // independent writes: dimensions (outside SubtypeMetadata, via
        // set_image_dimensions) and title (via the shared update_metadata,
        // same as audio). Neither write's failure blocks the other or fails
        // indexing.
        if file_type == FileType::Image {
            if let Some(tags) = self.image_tags.read(&entry.path).await {
                if let (Some(width), Some(height)) = (tags.width, tags.height) {
                    if let Err(err) = self
                        .repo
                        .set_image_dimensions(file.uuid, width, height)
                        .await
                    {
                        tracing::warn!(
                            path = %entry.path,
                            error = %err,
                            "indexed but failed to write extracted image dimensions"
                        );
                    }
                }
                if let Some(title) = tags.title {
                    let metadata = crate::catalog::model::SubtypeMetadata::Image {
                        title: Some(title),
                        caption: None,
                    };
                    if let Err(err) = self.repo.update_metadata(file.uuid, &metadata).await {
                        tracing::warn!(
                            path = %entry.path,
                            error = %err,
                            "indexed but failed to write extracted image title"
                        );
                    }
                }
            }
        }
        Ok(true)
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p alexandria-core --test catalog -- index::`
Expected: all pass, including the 4 new tests. `cargo build --workspace` will still fail at this point (`services.rs` not yet updated) — that's expected, exactly as it was during the audio slice's Task 4. Verify your own two files' tests pass using the same temporary-local-patch-then-revert technique the audio slice's Task 4 used if you need real GREEN evidence without touching `services.rs`: temporarily add a placeholder 6th argument to `services.rs`'s `IndexHandler::new(...)` call (e.g. `alexandria_core::catalog::image_tags::ExifImageMetadataReader` doesn't exist as a bare value type outside this crate's own module — since you're already inside `alexandria-core`, `crate::catalog::image_tags::ExifImageMetadataReader` from Task 2 works fine as the temporary patch value), confirm GREEN, then `git checkout -- crates/alexandria-core/src/services.rs` before committing.

- [ ] **Step 5: `fmt` + `clippy`**

Run: `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` (the exact, full-workspace commands). Expected: the *only* errors, if any, come from `services.rs`'s now-outdated `IndexHandler::new(...)` call (Task 7's job) — paste the real output in your report showing this.

- [ ] **Step 6: Commit**

```bash
git add crates/alexandria-core/src/catalog/commands/index.rs crates/alexandria-core/tests/catalog/index.rs
git commit -m "feat: extract image EXIF data into dimensions and title on first index"
```

---

### Task 7: Wire `ExifImageMetadataReader` into `services.rs`

**Files:**
- Modify: `crates/alexandria-core/src/services.rs`

**Interfaces:**
- Consumes: `ExifImageMetadataReader` from Task 2, `IndexHandler<A, R, F, C, M, N>::new` from Task 6.

This fixes the compile break Task 6 deliberately left, exactly mirroring the audio slice's Task 5.

- [ ] **Step 1: Add the import**

In `crates/alexandria-core/src/services.rs`, find:

```rust
use crate::catalog::audio_tags::LoftyAudioMetadataReader;
use crate::catalog::commands::index::IndexHandler;
```

Change to:

```rust
use crate::catalog::audio_tags::LoftyAudioMetadataReader;
use crate::catalog::commands::index::IndexHandler;
use crate::catalog::image_tags::ExifImageMetadataReader;
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
>;
```

- [ ] **Step 3: Update the construction site**

Find:

```rust
    let audio_tags = LoftyAudioMetadataReader;
    let index_handler = Arc::new(IndexHandler::new(
        auth.clone(),
        repo.clone(),
        fs,
        clock,
        audio_tags,
    ));
```

Change to:

```rust
    let audio_tags = LoftyAudioMetadataReader;
    let image_tags = ExifImageMetadataReader;
    let index_handler = Arc::new(IndexHandler::new(
        auth.clone(),
        repo.clone(),
        fs,
        clock,
        audio_tags,
        image_tags,
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
git commit -m "feat: wire ExifImageMetadataReader into DefaultIndexHandler"
```

---

### Task 8: HTTP/FFI integration + parity test

**Files:**
- Modify: `crates/alexandria-ffi/tests/parity.rs`

**Interfaces:**
- Consumes: the full extraction pipeline through both surfaces (unchanged public API — this task only adds a new test).

Reuses the exact `local_settings()` / `seed_session()` / `build_services()` / `app()` / `alexandria_index_init` / `alexandria_index_start` / `alexandria_file_get_by_uuid` scaffolding every other test in this file uses. Reuses the same fixture-generation approach as Task 2's unit test (the `image` crate to encode a real JPEG, `little_exif` to write EXIF into it) rather than a checked-in binary. **Important:** this test must poll on the actual extraction write landing (query `images.width`/`images.height` directly), not just on file-row existence — the audio slice's final review found and fixed exactly this race in its own parity test; don't reintroduce it here.

- [ ] **Step 1: Add `image` and `little_exif` as FFI dev-dependencies**

In `crates/alexandria-ffi/Cargo.toml`'s `[dev-dependencies]` section, add:

```toml
image.workspace = true
little_exif.workspace = true
```

- [ ] **Step 2: Write the failing test**

Append to the end of `crates/alexandria-ffi/tests/parity.rs`:

```rust
/// Encode a tiny real JPEG (4x3 pixels) using the `image` crate — a real,
/// valid JPEG file, not hand-crafted bytes. Mirrors the identical helper in
/// `alexandria-core`'s `catalog::image_tags` unit tests.
fn write_minimal_jpeg(path: &std::path::Path) {
    let img = image::RgbImage::from_pixel(4, 3, image::Rgb([128, 64, 32]));
    img.save(path).expect("encode jpeg");
}

/// Write EXIF tags (pixel dimensions + a description) into an existing JPEG
/// using `little_exif`.
fn write_test_exif(path: &std::path::Path, width: u32, height: u32, description: &str) {
    use little_exif::exif_tag::ExifTag;
    use little_exif::metadata::Metadata;

    let mut metadata = Metadata::new();
    metadata.set_tag(ExifTag::ImageDescription(description.to_string()));
    metadata.set_tag(ExifTag::PixelXDimension(vec![width]));
    metadata.set_tag(ExifTag::PixelYDimension(vec![height]));
    metadata.write_to_file(path).expect("write exif");
}

/// Poll until `images.width`/`images.height` are both non-NULL for the
/// named file — proves the extraction write landed, not just that the file
/// row exists (the audio slice's final review found and fixed exactly this
/// race for its own parity test; this test avoids repeating it).
async fn wait_for_http_image_dimensions(pool: &sqlx::sqlite::SqlitePool, name: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let row: Option<(Option<i64>, Option<i64>)> = sqlx::query_as(
            "SELECT images.width, images.height FROM images \
             JOIN files ON files.id = images.file_id \
             WHERE files.name = ?",
        )
        .bind(name)
        .fetch_optional(pool)
        .await
        .unwrap();
        if let Some((Some(_), Some(_))) = row {
            return;
        }
        if std::time::Instant::now() > deadline {
            panic!("http never wrote extracted image dimensions");
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Issue #44 image slice parity — index a tagged JPEG through both
/// transports and assert the extracted dimensions + title (written by the
/// indexer itself, not by a manual PATCH) are byte-for-byte identical.
#[tokio::test]
async fn given_tagged_image_file_when_indexed_via_http_and_ffi_then_extracted_metadata_matches() {
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    let http_photo = http_lib.path().join("photo.jpg");
    write_minimal_jpeg(&http_photo);
    write_test_exif(&http_photo, 800, 600, "Parity Description");

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
    wait_for_http_image_dimensions(&http_pool, "photo.jpg").await;

    let (http_uuid,): (String,) = sqlx::query_as("SELECT uuid FROM files WHERE name = ?")
        .bind("photo.jpg")
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
    let ffi_photo = ffi_lib.path().join("photo.jpg");
    write_minimal_jpeg(&ffi_photo);
    write_test_exif(&ffi_photo, 800, 600, "Parity Description");
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

        // Poll the FFI leg's own sqlite file directly for the extraction
        // write, same as the HTTP leg — not just file-row existence.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ffi_uuid: String = rt.block_on(async {
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect(&format!("sqlite://{ffi_db_for_poll}?mode=rw"))
                .await
                .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let row: Option<(String, Option<i64>, Option<i64>)> = sqlx::query_as(
                    "SELECT files.uuid, images.width, images.height FROM images \
                     JOIN files ON files.id = images.file_id \
                     WHERE files.name = ?",
                )
                .bind("photo.jpg")
                .fetch_optional(&pool)
                .await
                .unwrap();
                if let Some((uuid, Some(_), Some(_))) = row {
                    return uuid;
                }
                if std::time::Instant::now() > deadline {
                    panic!("ffi never wrote extracted image dimensions");
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
    assert_eq!(http_body["width"], ffi_body["width"]);
    assert_eq!(http_body["height"], ffi_body["height"]);
    assert_eq!(http_body["metadata"], ffi_body["metadata"]);
    assert_eq!(http_body["width"], 800);
    assert_eq!(http_body["height"], 600);
    assert_eq!(http_body["metadata"]["title"], "Parity Description");
}
```

Check that a helper function named `wait_for_http_image_dimensions` or similar doesn't already exist under a different name in this file before adding it (search: `grep -n "fn wait_for_http" crates/alexandria-ffi/tests/parity.rs`) — reuse the existing one if a prior task in a different slice already added an equivalent, rather than duplicating it.

- [ ] **Step 3: Run test to verify it passes**

Run: `cargo test -p alexandria-ffi --test parity given_tagged_image_file_when_indexed_via_http_and_ffi -- --nocapture`
Expected: `test result: ok. 1 passed`. As with the audio slice's equivalent task, this only exercises code paths built in Tasks 1–7, so there's no meaningful "write it failing first" step — the assertions either hold given the prior tasks' implementation, or reveal a real bug in it.

- [ ] **Step 4: Run the full parity suite to confirm no regression**

Run: `cargo test -p alexandria-ffi --test parity`
Expected: every test in the file passes.

- [ ] **Step 5: `fmt` + `clippy`**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings` (exact literal commands). Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/alexandria-ffi/Cargo.toml crates/alexandria-ffi/tests/parity.rs
git commit -m "test: add HTTP/FFI parity coverage for extracted image metadata"
```

---

### Task 9: Full verification, PR, and merge

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
git push -u origin feature/image-metadata-extraction
```

```bash
gh pr create --title "feat: extract image metadata during indexing (issue #44 image slice)" --body "$(cat <<'EOF'
## Summary
- Implements the image slice of issue #44: indexing now reads embedded EXIF data via `kamadak-exif` and pre-populates pixel dimensions and (when present) title, instead of leaving every field for the owner to enter manually.
- Extraction runs once, at first index only; `refresh.rs` is untouched. Extraction failure (no EXIF, corrupt file, unsupported format) never fails the indexing run and is never counted in `IndexOutcome::failed`.
- Unlike the audio slice, EXIF's most reliable data (pixel dimensions) lives outside `SubtypeMetadata::Image` entirely, so this adds one narrow new repository write method (`set_image_dimensions`) and one narrow new read method (`find_image_dimensions`), plus two new `FileView` fields — the read path didn't exist before this slice; see the design doc's amendment.
- `caption` stays owner-only (no EXIF-native field maps to it); gif/webp/bmp/svg have no EXIF and always yield no metadata (same graceful degradation as audio's `.wma`).
- Document/video extraction are separate follow-up slices, in that order.

See \`docs/superpowers/specs/2026-08-06-image-metadata-extraction-design.md\` for the full design.

Relates to #44 (does not close it — this is the image slice only).

## Test plan
- [x] \`cargo test --workspace\` — all green
- [x] \`cargo fmt --all\` / \`cargo clippy --workspace --all-targets -- -D warnings\`
- [x] Unit tests: \`ExifImageMetadataReader\` against a real generated JPEG+EXIF fixture (tagged/untagged/missing-file), repository \`set_image_dimensions\`/\`find_image_dimensions\`, \`BrowseFilesHandler::get_by_uuid\` width/height wiring, \`IndexHandler\` against \`FakeImageMetadataReader\` (dimensions+title/dimensions-only/untagged/non-image, with a call-count assertion proving the reader is never consulted for non-image files)
- [x] HTTP/FFI parity test: index a real tagged JPEG through both surfaces, assert extracted dimensions + title match (race-free — both legs poll on the actual extraction write, not just file-row existence)

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 3: Wait for CI, then merge**

Run: `gh pr checks <PR number> --watch`
Expected: all checks pass.

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
