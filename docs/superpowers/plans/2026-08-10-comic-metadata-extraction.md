# Comic Metadata Extraction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extract embedded comic metadata (title, series, issue number, page count) during first-index for `.cbz` archives, prefilling `SubtypeMetadata::Comic`'s owner-editable columns and a new `comic_page_count` `FileView` field, via a `zip`+`ComicInfo.xml`-backed reader.

**Architecture:** A new `ComicMetadataReader` trait port with a concrete `CbzComicMetadataReader` implementation becomes `IndexHandler`'s 9th generic collaborator. `title`/`series`/`issue_number` reuse the existing owner-editable `comic_books` columns via `update_metadata`; `page_count` needs a new `CatalogRepository::set_comic_page_count`/`find_comic_page_count` method pair and a new `FileView` field — mirroring image's `width`/`height`, document's `page_count`, and video's `duration_seconds`. This is the fifth and final slice of issue #44.

**Tech Stack:** Rust, `zip` (already a workspace dependency), `quick-xml` (new dependency), sqlx/SQLite, tokio.

## Global Constraints

- Format scope: **CBZ only**. `classify_by_extension` maps cbr/cbz to `FileType::Comic`; `.cbr` always yields `None` from the reader — no attempt to parse it.
- Metadata source: `ComicInfo.xml` supplies `title`/`series`/`issue_number` when present (matched case-insensitively: `ComicInfo.xml`, `comicinfo.xml`, etc.); `page_count` is **always** computed by counting image-extension entries (`.jpg`/`.jpeg`/`.png`/`.gif`/`.webp`/`.bmp`) in the archive, independent of whether `ComicInfo.xml` exists at all.
- `issue_number` parsing from `ComicInfo.xml`'s `<Number>` element is best-effort: a value that doesn't parse as `i64` (e.g. `"1.5"`, `"Annual"`) leaves `issue_number` as `None`, never an error.
- `title`/`series`/`issue_number` reuse the existing owner-editable `comic_books` columns via `update_metadata`; `page_count` needs its own new repository method pair — **no new migration**, `comic_books.page_count` already exists in the original schema.
- `FileView`'s new field is named **`comic_page_count`**, not `page_count` — document's slice already claimed `page_count` for its own field on the same struct, and reusing the name would make the two ambiguous to API consumers.
- Extraction runs **once, at first index only**. Never touch `refresh.rs`.
- Extraction failure (unopenable zip, missing/malformed `ComicInfo.xml`) is **never** a run failure: only an unopenable archive collapses the whole result to `None`; a missing/malformed `ComicInfo.xml` still yields a partial `Some` result (page_count present, the other three `None`).
- The `set_comic_page_count` write and the `title`/`series`/`issue_number` write (via `update_metadata`) are **independent** — a failure in one must not block or be conflated with the other, and neither fails indexing.
- The metadata write fires whenever ANY of `title`/`series`/`issue_number` is `Some` — the same "not all-empty" gate shape as audio/image/video (there is no comic analogue of document's always-present `format_kind`, so this gate is a genuine 3-way check, never trivially true).
- Every new/changed Rust file must pass `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` before its task is done — report the **exact literal commands**, not narrower or `--check`-only variants (except Task 10's final verification, which explicitly uses `--check` to *verify* rather than reformat).
- `zip`'s exact method/type names (for reading and writing archives) and `quick-xml`'s exact event/reader API are best-effort based on their documented APIs at the time of writing; if a name has moved in the resolved version, fix it against `cargo doc -p <crate> --open` — this is the same situation four already-shipped slices handled successfully.
- Branch: `feature/comic-metadata-extraction` off `main`. One PR at the end of Task 10, following this repo's established branch → PR → CI → squash-merge cycle.

---

### Task 1: `ComicTags` type and `ComicMetadataReader` trait

**Files:**
- Create: `crates/alexandria-core/src/catalog/comic_tags.rs`
- Modify: `crates/alexandria-core/src/catalog/mod.rs`

**Interfaces:**
- Produces: `pub struct ComicTags { pub title: Option<String>, pub series: Option<String>, pub issue_number: Option<i64>, pub page_count: Option<i64> }`
- Produces: `#[allow(async_fn_in_trait)] pub trait ComicMetadataReader: Send + Sync { async fn read(&self, path: &str) -> Option<ComicTags>; }`

Pure logic, no I/O, no new dependency yet — mirrors all four prior slices' Task 1 exactly in shape.

- [ ] **Step 1: Write the file**

Create `crates\alexandria-core\src\catalog\comic_tags.rs`:

```rust
/// Tags read from a comic archive's embedded metadata (`ComicInfo.xml`,
/// the de-facto ComicRack/ComicVine standard, plus an archive-entry image
/// count — issue #44 comic slice). `page_count` is always computed by
/// counting image entries in the archive, independent of whether
/// `ComicInfo.xml` exists at all; `title`/`series`/`issue_number` come
/// only from `ComicInfo.xml` and are `None` when it's absent or
/// unparseable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComicTags {
    pub title: Option<String>,
    pub series: Option<String>,
    pub issue_number: Option<i64>,
    pub page_count: Option<i64>,
}

#[allow(async_fn_in_trait)]
pub trait ComicMetadataReader: Send + Sync {
    /// Best-effort read of embedded comic metadata. `None` covers only
    /// "couldn't open the archive at all" — a readable archive with no
    /// `ComicInfo.xml` still yields `Some` with `page_count` set and the
    /// other three fields `None`.
    async fn read(&self, path: &str) -> Option<ComicTags>;
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
pub mod video_tags;
```

Change to (alphabetical: `comic_tags` after `commands`, before `document_tags`):

```rust
pub mod audio_tags;
pub mod classify;
pub mod clock;
pub mod comic_tags;
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
git add crates/alexandria-core/src/catalog/comic_tags.rs crates/alexandria-core/src/catalog/mod.rs
git commit -m "feat: add ComicTags and ComicMetadataReader port"
```

---

### Task 2: `CbzComicMetadataReader` (real implementation)

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Modify: `crates/alexandria-core/Cargo.toml`
- Modify: `crates/alexandria-core/src/catalog/comic_tags.rs`

**Interfaces:**
- Consumes: `ComicTags`, `ComicMetadataReader` from Task 1.
- Produces: `#[derive(Debug, Default, Clone, Copy)] pub struct CbzComicMetadataReader;` implementing `ComicMetadataReader`.

This is the riskiest task in the plan: it depends on the `zip` crate (already a workspace dependency, used for EPUB fixtures — read here for the first time in production code) plus a brand-new `quick-xml` dependency for the `ComicInfo.xml` parse.

- [ ] **Step 1: Add the dependency**

In `Cargo.toml` (workspace root), `[workspace.dependencies]` — insert `quick-xml` alphabetically between `lopdf` and `reqwest`:

```toml
chrono = { version = "0.4", features = ["serde"] }
epub = "2"
ffmpeg-next = "9"
jsonwebtoken = "9"
kamadak-exif = "0.5"
lofty = "0.22"
lopdf = "0.34"
quick-xml = "0.37"
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
```

In `crates/alexandria-core/Cargo.toml`'s `[dependencies]` — insert `quick-xml` alphabetically after `lopdf`, before `reqwest`, and add `zip.workspace = true` (currently only a dev-dependency; comic extraction needs it in production code too):

```toml
anyhow.workspace = true
argon2.workspace = true
chrono.workspace = true
epub.workspace = true
ffmpeg-next.workspace = true
jsonwebtoken.workspace = true
kamadak-exif.workspace = true
lofty.workspace = true
lopdf.workspace = true
quick-xml.workspace = true
reqwest.workspace = true
serde.workspace = true
serde_json.workspace = true
sha2.workspace = true
sqlx.workspace = true
thiserror.workspace = true
tokio.workspace = true
toml.workspace = true
tracing.workspace = true
uuid.workspace = true
walkdir.workspace = true
zip.workspace = true
```

(`zip` was already in `[dev-dependencies]` for EPUB test fixtures — leave that entry as-is; adding it to `[dependencies]` too is normal and expected, cargo handles a crate appearing in both sections of the same manifest.)

Run: `cargo build -p alexandria-core`
Expected: builds successfully. `Cargo.lock` updates. If `quick-xml` doesn't resolve exactly as pinned, adjust the version to the latest available 0.37.x/0.38.x release and note the change in your report — this is a normal dependency-resolution step, not a plan defect.

- [ ] **Step 2: Write the failing test**

Append to `crates/alexandria-core/src/catalog/comic_tags.rs`, inside a new `#[cfg(test)] mod tests` block at the end of the file:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal valid CBZ with the `zip` crate itself: an optional
    /// `ComicInfo.xml` entry plus `page_count` dummy `.jpg` entries (each
    /// just a few bytes — the reader only counts entries by extension, it
    /// never decodes image data). This is a real, valid ZIP archive — not
    /// hand-crafted bytes.
    fn write_minimal_cbz(
        path: &std::path::Path,
        comic_info: Option<(&str, &str, &str)>,
        page_count: usize,
    ) {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let file = std::fs::File::create(path).expect("create cbz file");
        let mut zip = zip::ZipWriter::new(file);
        let options = SimpleFileOptions::default();

        if let Some((title, series, number)) = comic_info {
            zip.start_file("ComicInfo.xml", options)
                .expect("start ComicInfo.xml");
            let xml = format!(
                r#"<?xml version="1.0"?>
<ComicInfo xmlns:xsd="http://www.w3.org/2001/XMLSchema" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <Title>{title}</Title>
  <Series>{series}</Series>
  <Number>{number}</Number>
</ComicInfo>"#
            );
            zip.write_all(xml.as_bytes()).expect("write ComicInfo.xml");
        }

        for i in 0..page_count {
            zip.start_file(format!("page-{i:03}.jpg"), options)
                .expect("start page");
            zip.write_all(b"not-a-real-jpeg-just-bytes")
                .expect("write page");
        }

        zip.finish().expect("finish cbz zip");
    }

    #[tokio::test]
    async fn given_tagged_cbz_when_read_then_title_series_issue_and_page_count_extracted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tagged.cbz");
        write_minimal_cbz(&path, Some(("Test Title", "Test Series", "3")), 5);

        let reader = CbzComicMetadataReader;
        let tags = reader
            .read(path.to_str().unwrap())
            .await
            .expect("tags extracted");

        assert_eq!(tags.title.as_deref(), Some("Test Title"));
        assert_eq!(tags.series.as_deref(), Some("Test Series"));
        assert_eq!(tags.issue_number, Some(3));
        assert_eq!(tags.page_count, Some(5));
    }

    #[tokio::test]
    async fn given_cbz_without_comicinfo_when_read_then_only_page_count_extracted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("untagged.cbz");
        write_minimal_cbz(&path, None, 7);

        let reader = CbzComicMetadataReader;
        let tags = reader
            .read(path.to_str().unwrap())
            .await
            .expect("tags extracted");

        assert_eq!(tags.title, None);
        assert_eq!(tags.series, None);
        assert_eq!(tags.issue_number, None);
        assert_eq!(
            tags.page_count,
            Some(7),
            "page_count must be computed even with no ComicInfo.xml"
        );
    }

    #[tokio::test]
    async fn given_cbz_with_non_integer_issue_number_when_read_then_issue_number_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("annual.cbz");
        write_minimal_cbz(&path, Some(("Annual Special", "Test Series", "Annual")), 1);

        let reader = CbzComicMetadataReader;
        let tags = reader
            .read(path.to_str().unwrap())
            .await
            .expect("tags extracted");

        assert_eq!(tags.title.as_deref(), Some("Annual Special"));
        assert_eq!(
            tags.issue_number, None,
            "a non-integer <Number> must not error, just leave issue_number None"
        );
    }

    #[tokio::test]
    async fn given_missing_file_when_read_then_none_not_panic() {
        let reader = CbzComicMetadataReader;

        let tags = reader.read("/no/such/file.cbz").await;

        assert!(tags.is_none());
    }

    #[tokio::test]
    async fn given_unsupported_extension_when_read_then_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("book.cbr");
        std::fs::write(&path, b"not a real cbr file").expect("write stub");

        let reader = CbzComicMetadataReader;
        let tags = reader.read(path.to_str().unwrap()).await;

        assert!(tags.is_none(), ".cbr is not attempted at all");
    }
}
```

If `zip`'s `ZipWriter`/`SimpleFileOptions`/`start_file`/`finish` API, or `quick-xml`'s reader API (used in Step 4 below) don't match the resolved versions' actual APIs, adapt via `cargo doc -p zip --open` / `cargo doc -p quick-xml --open` — keep the same intent (a minimal valid CBZ with an optional `ComicInfo.xml` entry and N dummy image entries).

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p alexandria-core --lib catalog::comic_tags`
Expected: fails to compile — `CbzComicMetadataReader` does not exist yet.

- [ ] **Step 4: Implement `CbzComicMetadataReader`**

Add above the `#[cfg(test)]` block in `comic_tags.rs`:

```rust
/// Real comic reader covering `.cbz` (ZIP-based) archives — 1 of the 2
/// extensions `classify_by_extension` maps to `FileType::Comic`. `.cbr`
/// (RAR, proprietary, no viable pure-Rust reader) always yields `None` —
/// the same graceful degradation the document slice established for
/// `.mobi`/`.azw`/`.azw3`. `title`/`series`/`issue_number` come from a
/// `ComicInfo.xml` entry when present (matched case-insensitively);
/// `page_count` is always the count of image-extension entries in the
/// archive, regardless of whether `ComicInfo.xml` exists.
#[derive(Debug, Default, Clone, Copy)]
pub struct CbzComicMetadataReader;

const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "gif", "webp", "bmp"];

impl CbzComicMetadataReader {
    fn read_cbz(path: &str) -> Option<ComicTags> {
        let file = std::fs::File::open(path).ok()?;
        let mut archive = zip::ZipArchive::new(file).ok()?;

        let mut page_count = 0i64;
        let mut comic_info_index: Option<usize> = None;
        for i in 0..archive.len() {
            let entry = archive.by_index(i).ok()?;
            let name = entry.name().to_string();
            if name.eq_ignore_ascii_case("ComicInfo.xml") {
                comic_info_index = Some(i);
                continue;
            }
            let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
            if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
                page_count += 1;
            }
        }

        let (title, series, issue_number) = match comic_info_index {
            Some(i) => {
                let mut entry = archive.by_index(i).ok()?;
                let mut xml = String::new();
                std::io::Read::read_to_string(&mut entry, &mut xml).ok()?;
                parse_comic_info(&xml)
            }
            None => (None, None, None),
        };

        Some(ComicTags {
            title,
            series,
            issue_number,
            page_count: Some(page_count),
        })
    }
}

/// Parse `<Title>`/`<Series>`/`<Number>` out of a `ComicInfo.xml` document.
/// Malformed XML or a missing element collapses that field to `None`
/// rather than erroring — the caller already treats every field as
/// best-effort.
fn parse_comic_info(xml: &str) -> (Option<String>, Option<String>, Option<i64>) {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut title = None;
    let mut series = None;
    let mut issue_number = None;
    let mut current_tag = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                current_tag = String::from_utf8_lossy(e.name().as_ref()).into_owned();
            }
            Ok(Event::Text(e)) => {
                let text = e.unescape().unwrap_or_default().into_owned();
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                match current_tag.as_str() {
                    "Title" => title = Some(text.to_string()),
                    "Series" => series = Some(text.to_string()),
                    "Number" => issue_number = text.parse::<i64>().ok(),
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    (title, series, issue_number)
}

impl ComicMetadataReader for CbzComicMetadataReader {
    async fn read(&self, path: &str) -> Option<ComicTags> {
        let lower = path.to_ascii_lowercase();
        if lower.ends_with(".cbz") {
            Self::read_cbz(path)
        } else {
            None
        }
    }
}
```

If `zip::ZipArchive::new`/`len`/`by_index`/`Entry::name`, or `quick_xml::reader::Reader::from_str`/`config_mut().trim_text`/`read_event`/`events::Event` don't match the resolved versions' actual APIs, adapt via `cargo doc -p zip --open` / `cargo doc -p quick-xml --open`. Keep the same intent: iterate every zip entry once to (a) find a case-insensitive `ComicInfo.xml` match and (b) count image-extension entries; parse `<Title>`/`<Series>`/`<Number>` from the XML text content, with any parse or read failure collapsing that specific field (or the whole XML result) to `None` rather than propagating an error.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p alexandria-core --lib catalog::comic_tags`
Expected: `test result: ok. 5 passed; 0 failed`

- [ ] **Step 6: `fmt` + `clippy`**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings` (the exact, full-workspace commands).
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock crates/alexandria-core/Cargo.toml crates/alexandria-core/src/catalog/comic_tags.rs
git commit -m "feat: implement CbzComicMetadataReader"
```

---

### Task 3: `FakeComicMetadataReader` test double

**Files:**
- Modify: `crates/alexandria-core/tests/common/mod.rs`

**Interfaces:**
- Consumes: `ComicMetadataReader`, `ComicTags` from `alexandria_core::catalog::comic_tags`.
- Produces: `FakeComicMetadataReader::new()`, `.seed(path: &str, tags: ComicTags)`, `.call_count()`, implementing `ComicMetadataReader`.

Mirrors `FakeAudioMetadataReader`/`FakeImageMetadataReader`/`FakeDocumentMetadataReader`/`FakeVideoMetadataReader` (already in this file) exactly, including the call-count pattern.

- [ ] **Step 1: Add the fake**

Add this import in `crates/alexandria-core/tests/common/mod.rs`, alongside the existing `alexandria_core::catalog::document_tags::...` / `alexandria_core::catalog::video_tags::...` imports:

```rust
use alexandria_core::catalog::audio_tags::{AudioMetadataReader, AudioTags};
use alexandria_core::catalog::comic_tags::{ComicMetadataReader, ComicTags};
use alexandria_core::catalog::document_tags::{DocumentMetadataReader, DocumentTags};
use alexandria_core::catalog::image_tags::{ImageMetadataReader, ImageTags};
use alexandria_core::catalog::video_tags::{VideoMetadataReader, VideoTags};
```

(alphabetical: `comic_tags` sorts between `audio_tags` and `document_tags`)

Append this new fake at the end of the file, after `FakeVideoMetadataReader`'s `impl VideoMetadataReader for FakeVideoMetadataReader` block:

```rust
/// In-memory comic reader (issue #44 comic slice). `read()` answers
/// `None` for any path with no seeded tags, mirroring "couldn't open
/// archive / unsupported extension" — the same outcome
/// `CbzComicMetadataReader` produces for those cases. Also counts calls,
/// so a test can assert the reader was never consulted at all (e.g. for
/// a non-comic file).
#[derive(Debug, Default, Clone)]
pub struct FakeComicMetadataReader {
    tags: Arc<Mutex<HashMap<String, ComicTags>>>,
    call_count: Arc<Mutex<usize>>,
}

impl FakeComicMetadataReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the tags `read()` returns for `path`.
    pub fn seed(&self, path: &str, tags: ComicTags) -> &Self {
        self.tags.lock().unwrap().insert(path.to_string(), tags);
        self
    }

    /// How many times `read()` has been called.
    pub fn call_count(&self) -> usize {
        *self.call_count.lock().unwrap()
    }
}

impl ComicMetadataReader for FakeComicMetadataReader {
    async fn read(&self, path: &str) -> Option<ComicTags> {
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
git commit -m "test: add FakeComicMetadataReader test double"
```

---

### Task 4: Repository methods `set_comic_page_count` and `find_comic_page_count`

**Files:**
- Modify: `crates/alexandria-core/src/catalog/repos.rs`
- Modify: `crates/alexandria-core/tests/common/mod.rs`

**Interfaces:**
- Produces (on `CatalogRepository` trait and its `SqliteCatalogRepository`/`FakeCatalogRepository` implementations):
  - `async fn set_comic_page_count(&self, uuid: Uuid, page_count: i64) -> Result<(), DomainError>`
  - `async fn find_comic_page_count(&self, uuid: Uuid) -> Result<Option<i64>, DomainError>`
- Produces on `FakeCatalogRepository`: `pub fn comic_page_count_for(&self, uuid: Uuid) -> Option<i64>` (test inspector, mirrors the existing `document_page_count_for`).

Mirrors document's `set_document_page_count`/`find_document_page_count` exactly in shape. No migration needed — `comic_books.page_count` already exists in the original schema.

- [ ] **Step 1: Add the trait methods**

In `crates/alexandria-core/src/catalog/repos.rs`, in the `CatalogRepository` trait, add these two methods right after the existing `find_video_duration` method:

```rust
    /// Write a comic file's page count (issue #44 comic slice). Unlike
    /// `update_metadata`, this touches `comic_books.page_count` directly —
    /// `SubtypeMetadata::Comic` deliberately excludes it because it is not
    /// owner-editable (UC-04). Returns `NotFound` when no file row carries
    /// the UUID, `InvalidInput` when the file is not a comic.
    async fn set_comic_page_count(&self, uuid: Uuid, page_count: i64) -> Result<(), DomainError>;

    /// Read a comic file's page count, if set (issue #44 comic slice).
    /// `None` when the file doesn't exist, isn't a comic, or the column is
    /// still `NULL` (extraction never ran, or the archive couldn't be
    /// opened).
    async fn find_comic_page_count(&self, uuid: Uuid) -> Result<Option<i64>, DomainError>;
```

- [ ] **Step 2: Add the fakes**

In `crates/alexandria-core/tests/common/mod.rs`, add a new field to `FakeCatalogRepository`'s struct definition — it currently ends with:

```rust
    /// Duration (seconds) last written for `uuid` via `set_video_duration`
    /// (issue #44 video slice).
    video_durations: Arc<Mutex<HashMap<Uuid, f64>>>,
}
```

Change to:

```rust
    /// Duration (seconds) last written for `uuid` via `set_video_duration`
    /// (issue #44 video slice).
    video_durations: Arc<Mutex<HashMap<Uuid, f64>>>,
    /// Page count last written for `uuid` via `set_comic_page_count`
    /// (issue #44 comic slice).
    comic_page_counts: Arc<Mutex<HashMap<Uuid, i64>>>,
}
```

Add an inspector method in `impl FakeCatalogRepository`, right after the existing `video_duration_for` method:

```rust
    /// Page count last written for `uuid` via `set_comic_page_count`.
    /// `None` means no call has landed for that file yet.
    pub fn comic_page_count_for(&self, uuid: Uuid) -> Option<i64> {
        self.comic_page_counts.lock().unwrap().get(&uuid).copied()
    }
```

Add the two trait method implementations in `impl CatalogRepository for FakeCatalogRepository`, right after the existing `find_video_duration` implementation:

```rust
    async fn set_comic_page_count(
        &self,
        uuid: Uuid,
        page_count: i64,
    ) -> Result<(), DomainError> {
        let files = self.files.lock().unwrap();
        let file = files
            .values()
            .find(|f| f.uuid == uuid)
            .ok_or(DomainError::NotFound)?;
        if file.file_type != alexandria_core::catalog::model::FileType::Comic {
            return Err(DomainError::InvalidInput("file is not a comic".into()));
        }
        drop(files);
        self.comic_page_counts
            .lock()
            .unwrap()
            .insert(uuid, page_count);
        Ok(())
    }

    async fn find_comic_page_count(&self, uuid: Uuid) -> Result<Option<i64>, DomainError> {
        let files = self.files.lock().unwrap();
        let file = match files.values().find(|f| f.uuid == uuid) {
            Some(f) => f,
            None => return Ok(None),
        };
        if file.file_type != alexandria_core::catalog::model::FileType::Comic {
            return Ok(None);
        }
        drop(files);
        Ok(self.comic_page_counts.lock().unwrap().get(&uuid).copied())
    }
```

- [ ] **Step 3: Confirm the fakes compile**

Run: `cargo test -p alexandria-core --test catalog -- --list`
Expected: compiles cleanly. `cargo build -p alexandria-core` will still fail at this point — the trait now has two new required methods and `SqliteCatalogRepository` doesn't implement them yet. That's expected; the next step fixes it.

- [ ] **Step 4: Implement the real Sqlite methods**

In `crates/alexandria-core/src/catalog/repos.rs`, in `impl CatalogRepository for SqliteCatalogRepository`, add these two methods right after the existing `find_video_duration` implementation:

```rust
    async fn set_comic_page_count(
        &self,
        uuid: Uuid,
        page_count: i64,
    ) -> Result<(), DomainError> {
        let mut tx = self.pool.begin().await?;

        let (id, type_str): (i64, String) =
            sqlx::query_as("SELECT id, type FROM files WHERE uuid = ?")
                .bind(uuid.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(DomainError::NotFound)?;

        let actual_type = parse_type_str(&type_str)?;
        if actual_type != FileType::Comic {
            return Err(DomainError::InvalidInput("file is not a comic".into()));
        }

        let affected = sqlx::query("UPDATE comic_books SET page_count = ? WHERE file_id = ?")
            .bind(page_count)
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();

        if affected == 0 {
            return Err(DomainError::internal(format!(
                "subtype row missing for file {uuid} (comic)"
            )));
        }

        tx.commit().await?;
        Ok(())
    }

    async fn find_comic_page_count(&self, uuid: Uuid) -> Result<Option<i64>, DomainError> {
        let row: Option<(i64, String)> =
            sqlx::query_as("SELECT id, type FROM files WHERE uuid = ?")
                .bind(uuid.to_string())
                .fetch_optional(&self.pool)
                .await?;
        let (id, type_str) = match row {
            Some(r) => r,
            None => return Ok(None),
        };
        if parse_type_str(&type_str)? != FileType::Comic {
            return Ok(None);
        }

        let row: Option<(Option<i64>,)> =
            sqlx::query_as("SELECT page_count FROM comic_books WHERE file_id = ?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;

        Ok(row.and_then(|(pc,)| pc))
    }
```

- [ ] **Step 5: Verify the fakes and the workspace build together**

Run: `cargo build --workspace`
Expected: builds cleanly.

Run: `cargo test -p alexandria-core --test catalog`
Expected: all existing catalog tests still pass.

- [ ] **Step 6: `fmt` + `clippy`**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/alexandria-core/src/catalog/repos.rs crates/alexandria-core/tests/common/mod.rs
git commit -m "feat: add set_comic_page_count and find_comic_page_count to CatalogRepository"
```

---

### Task 5: `FileView` comic_page_count and `BrowseFilesHandler::get_by_uuid` wiring

**Files:**
- Modify: `crates/alexandria-core/src/catalog/model.rs`
- Modify: `crates/alexandria-core/src/catalog/queries/browse.rs`
- Test: `crates/alexandria-core/tests/catalog/browse.rs`

**Interfaces:**
- Consumes: `CatalogRepository::find_comic_page_count` from Task 4.
- Produces: `FileView { file, metadata, width, height, page_count, duration_seconds, comic_page_count: Option<i64> }` (was `{ ..., duration_seconds }`).

This closes the read-path gap for comic, exactly mirroring video's Task 6 (which itself mirrored document's Task 5). No HTTP or FFI code needs to change — both already serialize `FileView` generically.

- [ ] **Step 1: Write the failing test**

`crates/alexandria-core/tests/catalog/browse.rs` already imports `FormatKind`, `MediaKind`, `existing_file_with_hash`, `FakeCatalogRepository`, `FakeAuth`, `handler`, `TOKEN` (used by the existing video tests added in the video slice — search for `given_video_with_extracted_duration_when_get_by_uuid_then_duration_present` for the exact pattern to follow). Add these 3 new tests near it, following that same pattern exactly:

```rust
#[tokio::test]
async fn given_comic_with_extracted_page_count_when_get_by_uuid_then_page_count_present() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/issue1.cbz", "issue1", FileType::Comic, "h");
    let uuid = file.uuid;
    repo.seed(file);
    repo.set_comic_page_count(uuid, 24).await.unwrap();

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    assert_eq!(view.comic_page_count, Some(24));
}

#[tokio::test]
async fn given_comic_with_no_extracted_page_count_when_get_by_uuid_then_page_count_none() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/issue1.cbz", "issue1", FileType::Comic, "h");
    let uuid = file.uuid;
    repo.seed(file);

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    assert_eq!(view.comic_page_count, None);
}

#[tokio::test]
async fn given_non_comic_file_when_get_by_uuid_then_comic_page_count_none() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/song.mp3", "song", FileType::Audio, "h");
    let uuid = file.uuid;
    repo.seed(file);

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    assert_eq!(view.comic_page_count, None);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p alexandria-core --test catalog -- browse::`
Expected: fails to compile — `FileView` has no `comic_page_count` field yet, and `get_by_uuid`'s return doesn't set it.

- [ ] **Step 3: Add the field to `FileView`**

In `crates/alexandria-core/src/catalog/model.rs`, find:

```rust
    /// Extracted duration in seconds (issue #44 video slice). `None` for
    /// every non-video file, and for a video file whose duration hasn't
    /// been extracted yet.
    pub duration_seconds: Option<f64>,
}
```

Change to:

```rust
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
```

- [ ] **Step 4: Wire the read in `BrowseFilesHandler::get_by_uuid`**

In `crates/alexandria-core/src/catalog/queries/browse.rs`, find:

```rust
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

Change to:

```rust
        // Issue #44 video slice: duration_seconds lives outside
        // `SubtypeMetadata` (see `find_video_duration`'s doc comment), so
        // it's fetched separately and only for video files.
        let duration_seconds = if file.file_type == FileType::Video {
            self.repo.find_video_duration(uuid).await?
        } else {
            None
        };

        // Issue #44 comic slice: comic_page_count lives outside
        // `SubtypeMetadata` (see `find_comic_page_count`'s doc comment),
        // so it's fetched separately and only for comic files.
        let comic_page_count = if file.file_type == FileType::Comic {
            self.repo.find_comic_page_count(uuid).await?
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
            comic_page_count,
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
git commit -m "feat: surface extracted comic page count through FileView"
```

---

### Task 6: Wire `ComicMetadataReader` into `IndexHandler`

**Files:**
- Modify: `crates/alexandria-core/src/catalog/commands/index.rs`
- Modify: `crates/alexandria-core/tests/catalog/index.rs`

**Interfaces:**
- Consumes: `ComicMetadataReader`, `ComicTags` from Task 1; `FakeComicMetadataReader` from Task 3; `CatalogRepository::set_comic_page_count` from Task 4.
- Produces: `IndexHandler<A, R, F, C, M, N, O, P, Q>` (was `<A, R, F, C, M, N, O, P>`) — `Q: ComicMetadataReader` is the new 9th parameter, with `pub fn new(auth: A, repo: R, fs: F, clock: C, audio_tags: M, image_tags: N, document_tags: O, video_tags: P, comic_tags: Q) -> Self` (was 8 params).

Mirrors the video slice's Task 7 exactly in shape: widening `IndexHandler`'s constructor arity means every existing call site needs the new argument — this file's own tests, plus (in Task 7) `services.rs`. This task deliberately leaves `services.rs` broken; fixing it is Task 7's job.

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
use alexandria_core::catalog::video_tags::{VideoDuration, VideoMetadataReader, VideoTags};
use alexandria_core::errors::DomainError;

use crate::common::{
    existing_file, fixed_clock, now, FakeAudioMetadataReader, FakeAuth, FakeCatalogRepository,
    FakeDocumentMetadataReader, FakeFilesystem, FakeImageMetadataReader, FakeVideoMetadataReader,
};
```

Change to:

```rust
use uuid::Uuid;

use alexandria_core::auth::AuthService;
use alexandria_core::catalog::audio_tags::{AudioMetadataReader, AudioTags};
use alexandria_core::catalog::classify::classify_by_extension;
use alexandria_core::catalog::clock::Clock;
use alexandria_core::catalog::comic_tags::{ComicMetadataReader, ComicTags};
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
    FakeComicMetadataReader, FakeDocumentMetadataReader, FakeFilesystem, FakeImageMetadataReader,
    FakeVideoMetadataReader,
};
```

**1b.** Change the `handler` helper function. It currently reads:

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

Change to:

```rust
#[allow(clippy::too_many_arguments)]
fn handler<A, R, F, C, M, N, O, P, Q>(
    auth: A,
    repo: R,
    fs: F,
    clock: C,
    audio_tags: M,
    image_tags: N,
    document_tags: O,
    video_tags: P,
    comic_tags: Q,
) -> IndexHandler<A, R, F, C, M, N, O, P, Q>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
    C: Clock,
    M: AudioMetadataReader,
    N: ImageMetadataReader,
    O: DocumentMetadataReader,
    P: VideoMetadataReader,
    Q: ComicMetadataReader,
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
        comic_tags,
    )
}
```

**1c.** Read the actual current content of `crates/alexandria-core/tests/catalog/index.rs` yourself before doing the next edit — every prior slice's equivalent task found the plan's call-site-count estimate had drifted slightly from the real file, because tasks are added between when a plan is written and when it is executed. As of this plan being written, there are 27 pre-existing calls to `handler(...)` in this file, each ending with `FakeVideoMetadataReader::new()` (11 of them) or a named `video_tags` variable (the rest) as the last argument before the closing `);` — confirm this against the real file first. Every existing call to `handler(...)` needs a 9th argument, `FakeComicMetadataReader::new()`, added as the new last argument before the closing `);`. There are two literal shapes to find-and-replace:

**Shape 1** — the call's last argument before the closing `);` is the literal `FakeVideoMetadataReader::new()`. Change every occurrence of:
```
        FakeVideoMetadataReader::new(),
    );
```
to:
```
        FakeVideoMetadataReader::new(),
        FakeComicMetadataReader::new(),
    );
```

**Shape 2** — the call's last argument before the closing `);` is a named `video_tags` variable (the tests that seed or inspect the video reader, building it as a variable earlier in the test body). Change every occurrence of:
```
        video_tags,
    );
```
to:
```
        video_tags,
        FakeComicMetadataReader::new(),
    );
```

After both replacements, verify no call site was missed: count every occurrence of `handler(` (a call, not the `fn handler` definition line — that one doesn't call itself) and confirm it equals the count of `FakeComicMetadataReader::new()` occurrences (every pre-existing call site now has exactly one, whether inline or — after Step 1d adds new comic-specific tests — as a named variable for those new tests specifically).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p alexandria-core --test catalog -- index::`
Expected: fails to compile — constructor arity mismatch, `comic_tags` field doesn't exist on `IndexHandler` yet.

- [ ] **Step 3: Implement the change in `index.rs`**

In `crates/alexandria-core/src/catalog/commands/index.rs`, make these exact edits.

Add to the imports:

```rust
use crate::catalog::audio_tags::AudioMetadataReader;
use crate::catalog::comic_tags::ComicMetadataReader;
use crate::catalog::document_tags::DocumentMetadataReader;
use crate::catalog::image_tags::ImageMetadataReader;
use crate::catalog::video_tags::VideoMetadataReader;
```

Change the struct + constructor from:

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

to:

```rust
pub struct IndexHandler<A, R, F, C, M, N, O, P, Q> {
    auth: A,
    repo: R,
    fs: F,
    clock: C,
    audio_tags: M,
    image_tags: N,
    document_tags: O,
    video_tags: P,
    comic_tags: Q,
}

impl<A, R, F, C, M, N, O, P, Q> IndexHandler<A, R, F, C, M, N, O, P, Q>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
    C: Clock,
    M: AudioMetadataReader,
    N: ImageMetadataReader,
    O: DocumentMetadataReader,
    P: VideoMetadataReader,
    Q: ComicMetadataReader,
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
        comic_tags: Q,
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
            comic_tags,
        }
    }
```

Add a new `FileType::Comic` branch at the end of `index_entry`, right after the existing `FileType::Video` branch and before the final `Ok(true)`:

```rust
        // Best-effort comic metadata prefill (issue #44 comic slice). Two
        // independent writes: page count (outside SubtypeMetadata, via
        // set_comic_page_count — always present once the archive opens)
        // and title/series/issue_number (via the shared update_metadata).
        // Neither write's failure blocks the other or fails indexing.
        if file_type == FileType::Comic {
            if let Some(tags) = self.comic_tags.read(&entry.path).await {
                if let Some(page_count) = tags.page_count {
                    if let Err(err) = self
                        .repo
                        .set_comic_page_count(file.uuid, page_count)
                        .await
                    {
                        tracing::warn!(
                            path = %entry.path,
                            error = %err,
                            "indexed but failed to write extracted comic page count"
                        );
                    }
                }
                if tags.title.is_some() || tags.series.is_some() || tags.issue_number.is_some() {
                    let metadata = crate::catalog::model::SubtypeMetadata::Comic {
                        title: tags.title,
                        series: tags.series,
                        issue_number: tags.issue_number,
                    };
                    if let Err(err) = self.repo.update_metadata(file.uuid, &metadata).await {
                        tracing::warn!(
                            path = %entry.path,
                            error = %err,
                            "indexed but failed to write extracted comic metadata"
                        );
                    }
                }
            }
        }
        Ok(true)
    }
}
```

(This replaces the file's existing final `Ok(true)\n    }\n}` — the new branch goes immediately before that line, after the existing `FileType::Video` block's closing `}`.)

**1d.** Add these new tests at the end of `crates/alexandria-core/tests/catalog/index.rs` (after the last existing test):

```rust
#[tokio::test]
async fn given_tagged_comic_when_execute_then_page_count_and_metadata_written() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.cbz", "a.cbz", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let comic_tags = FakeComicMetadataReader::new();
    comic_tags.seed(
        "/library/a.cbz",
        ComicTags {
            title: Some("A Comic".to_string()),
            series: Some("A Series".to_string()),
            issue_number: Some(3),
            page_count: Some(24),
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
        FakeVideoMetadataReader::new(),
        comic_tags,
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.cbz").expect("indexed");
    assert_eq!(repo_handle.comic_page_count_for(a.uuid), Some(24));
    let metadata = repo_handle
        .metadata_for(a.uuid)
        .expect("metadata written from extracted tags");
    assert_eq!(
        metadata,
        SubtypeMetadata::Comic {
            title: Some("A Comic".to_string()),
            series: Some("A Series".to_string()),
            issue_number: Some(3),
        }
    );
}

#[tokio::test]
async fn given_comic_with_page_count_but_no_other_fields_when_execute_then_only_page_count_written()
{
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.cbz", "a.cbz", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let comic_tags = FakeComicMetadataReader::new();
    comic_tags.seed(
        "/library/a.cbz",
        ComicTags {
            title: None,
            series: None,
            issue_number: None,
            page_count: Some(10),
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
        FakeVideoMetadataReader::new(),
        comic_tags,
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.cbz").expect("indexed");
    assert_eq!(repo_handle.comic_page_count_for(a.uuid), Some(10));
    assert!(
        repo_handle.metadata_for(a.uuid).is_none(),
        "no title/series/issue_number extracted means update_metadata is never called"
    );
}

#[tokio::test]
async fn given_comic_with_issue_number_but_no_page_count_when_execute_then_only_metadata_written()
{
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.cbz", "a.cbz", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let comic_tags = FakeComicMetadataReader::new();
    comic_tags.seed(
        "/library/a.cbz",
        ComicTags {
            title: None,
            series: None,
            issue_number: Some(7),
            page_count: None,
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
        FakeVideoMetadataReader::new(),
        comic_tags,
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.cbz").expect("indexed");
    assert_eq!(
        repo_handle.comic_page_count_for(a.uuid),
        None,
        "no page_count extracted means set_comic_page_count is never called"
    );
    let metadata = repo_handle
        .metadata_for(a.uuid)
        .expect("issue_number alone triggers the metadata write");
    assert_eq!(
        metadata,
        SubtypeMetadata::Comic {
            title: None,
            series: None,
            issue_number: Some(7),
        }
    );
}

#[tokio::test]
async fn given_unopenable_comic_file_when_execute_then_neither_write_happens() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.cbz", "a.cbz", "h-a")
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
        FakeComicMetadataReader::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.cbz").expect("indexed");
    assert_eq!(repo_handle.comic_page_count_for(a.uuid), None);
    assert!(repo_handle.metadata_for(a.uuid).is_none());
}

#[tokio::test]
async fn given_non_comic_file_when_execute_then_comic_reader_never_consulted() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/notes.md", "notes.md", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let audio_tags = FakeAudioMetadataReader::new();
    let image_tags = FakeImageMetadataReader::new();
    let document_tags = FakeDocumentMetadataReader::new();
    let video_tags = FakeVideoMetadataReader::new();
    let comic_tags = FakeComicMetadataReader::new();
    let comic_tags_handle = comic_tags.clone();
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        audio_tags,
        image_tags,
        document_tags,
        video_tags,
        comic_tags,
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    assert_eq!(
        comic_tags_handle.call_count(),
        0,
        "the comic reader must not be consulted at all for a non-comic file"
    );
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p alexandria-core --test catalog -- index::`
Expected: all pass, including the 5 new tests. `cargo build --workspace` will still fail at this point (`services.rs` not yet updated) — that's expected, exactly as it was during all four prior slices' equivalent task. If you need real GREEN evidence before `services.rs` is fixed, temporarily add a 9th argument to `services.rs`'s `IndexHandler::new(...)` call using `crate::catalog::comic_tags::CbzComicMetadataReader` (from Task 2 — works via its fully-qualified path even though `services.rs` doesn't import it yet), confirm GREEN, then `git checkout -- crates/alexandria-core/src/services.rs` before committing.

- [ ] **Step 5: `fmt` + `clippy`**

Run: `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` (the exact, full-workspace commands). Expected: the *only* errors, if any, come from `services.rs`'s now-outdated `IndexHandler::new(...)` call (Task 7's job) — paste the real output in your report showing this.

- [ ] **Step 6: Commit**

```bash
git add crates/alexandria-core/src/catalog/commands/index.rs crates/alexandria-core/tests/catalog/index.rs
git commit -m "feat: extract comic metadata into page count and subtype fields on first index"
```

---

### Task 7: Wire `CbzComicMetadataReader` into `services.rs`

**Files:**
- Modify: `crates/alexandria-core/src/services.rs`

**Interfaces:**
- Consumes: `CbzComicMetadataReader` from Task 2, `IndexHandler<A, R, F, C, M, N, O, P, Q>::new` from Task 6.

Fixes the compile break Task 6 deliberately left, exactly mirroring all four prior slices' equivalent task.

- [ ] **Step 1: Add the import**

In `crates/alexandria-core/src/services.rs`, find this block (the file's imports are alphabetically ordered by full path):

```rust
use crate::catalog::audio_tags::LoftyAudioMetadataReader;
use crate::catalog::clock::SystemClock;
```

Change to insert the new import between them (`comic_tags` sorts alphabetically between `audio_tags` and `clock`):

```rust
use crate::catalog::audio_tags::LoftyAudioMetadataReader;
use crate::catalog::clock::SystemClock;
use crate::catalog::comic_tags::CbzComicMetadataReader;
```

(`comic_tags` sorts alphabetically between `clock` and `commands::edit_content` — "clock" < "comic_tags" < "commands" by the fourth character.)

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
    FfmpegVideoMetadataReader,
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
    CbzComicMetadataReader,
>;
```

- [ ] **Step 3: Update the construction site**

Find:

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

Change to:

```rust
    let audio_tags = LoftyAudioMetadataReader;
    let image_tags = ExifImageMetadataReader;
    let document_tags = PdfEpubMetadataReader;
    let video_tags = FfmpegVideoMetadataReader;
    let comic_tags = CbzComicMetadataReader;
    let index_handler = Arc::new(IndexHandler::new(
        auth.clone(),
        repo.clone(),
        fs,
        clock,
        audio_tags,
        image_tags,
        document_tags,
        video_tags,
        comic_tags,
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
git commit -m "feat: wire CbzComicMetadataReader into DefaultIndexHandler"
```

---

### Task 8: HTTP/FFI integration + parity test

**Files:**
- Modify: `crates/alexandria-ffi/Cargo.toml`
- Modify: `crates/alexandria-ffi/tests/parity.rs`

**Interfaces:**
- Consumes: the full extraction pipeline through both surfaces (unchanged public API — this task only adds a new test).

Reuses the exact `local_settings()` / `seed_session()` / `build_services()` / `app()` / `alexandria_index_init` / `alexandria_index_start` / `alexandria_file_get_by_uuid` scaffolding every other test in this file uses. Reuses the same fixture-generation approach as Task 2's unit tests (real CBZ built with the `zip` crate). **Both legs must poll on every column the test asserts on before proceeding to the GET/`alexandria_file_get_by_uuid` call** — the image slice's final review found and fixed a residual race where a test polled on one extraction write but asserted on a different, later one; every slice since has avoided repeating it. This slice writes up to two independent columns per file (`comic_books.page_count` in one transaction; `comic_books.title`/`series`/`issue_number` in another), so the wait condition must require every column the assertions check.

- [ ] **Step 1: Add `zip` as an FFI dev-dependency**

`zip` is NOT currently an `alexandria-ffi` dev-dependency (the document slice's own parity test used a PDF fixture, not EPUB, so it never needed `zip` there — only `alexandria-core` has it). In `crates/alexandria-ffi/Cargo.toml`'s `[dev-dependencies]` section (currently `alexandria-core`, `alexandria-http`, `axum`, `chrono`, `ffmpeg-next`, `image`, `little_exif`, `lofty`, `lopdf`, `serde_json`, `sqlx`, `tempfile`, `tokio`, `tower`), add at the end (alphabetically last):

```toml
zip.workspace = true
```

`quick-xml` is NOT needed here — the parity test only builds a CBZ fixture (writing, not parsing XML), it never needs to parse `ComicInfo.xml` itself. Do not add `quick-xml` as an FFI dependency.

- [ ] **Step 2: Write the test**

Append to the end of `crates/alexandria-ffi/tests/parity.rs`. This mirrors the video slice's `given_tagged_mp4_file_when_indexed_via_http_and_ffi_then_extracted_metadata_matches` test structure closely — read that test first (search for it in this file) to confirm the exact current shape of `local_settings()`, `seed_session()`, `setup_ffi_db()`, and the FFI-leg polling pattern (a `spawn_blocking` closure with its own `tokio::runtime::Runtime` connecting directly to the FFI database file), then write this new test following that same shape with the helpers below substituted in.

```rust
/// Build a minimal valid CBZ with the `zip` crate — mirrors the identical
/// helper in `alexandria-core`'s `catalog::comic_tags` unit tests.
fn write_minimal_cbz(path: &std::path::Path, title: &str, series: &str, number: &str, page_count: usize) {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let file = std::fs::File::create(path).expect("create cbz file");
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default();

    zip.start_file("ComicInfo.xml", options)
        .expect("start ComicInfo.xml");
    let xml = format!(
        r#"<?xml version="1.0"?>
<ComicInfo xmlns:xsd="http://www.w3.org/2001/XMLSchema" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <Title>{title}</Title>
  <Series>{series}</Series>
  <Number>{number}</Number>
</ComicInfo>"#
    );
    zip.write_all(xml.as_bytes()).expect("write ComicInfo.xml");

    for i in 0..page_count {
        zip.start_file(format!("page-{i:03}.jpg"), options)
            .expect("start page");
        zip.write_all(b"not-a-real-jpeg-just-bytes")
            .expect("write page");
    }

    zip.finish().expect("finish cbz zip");
}

/// Poll until `comic_books.title`/`comic_books.series`/
/// `comic_books.issue_number`/`comic_books.page_count` are all non-NULL
/// for the named file — proves BOTH extraction writes landed (metadata
/// write and page-count write are separate transactions), not just
/// file-row existence or a single write.
async fn wait_for_http_comic_extraction(pool: &sqlx::sqlite::SqlitePool, name: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let row: Option<(Option<String>, Option<String>, Option<i64>, Option<i64>)> = sqlx::query_as(
            "SELECT comic_books.title, comic_books.series, comic_books.issue_number, \
             comic_books.page_count \
             FROM comic_books \
             JOIN files ON files.id = comic_books.file_id \
             WHERE files.name = ?",
        )
        .bind(name)
        .fetch_optional(pool)
        .await
        .unwrap();
        if let Some((Some(_), Some(_), Some(_), Some(_))) = row {
            return;
        }
        if std::time::Instant::now() > deadline {
            panic!("http never wrote extracted comic metadata");
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Issue #44 comic slice parity — index a tagged CBZ through both
/// transports and assert the extracted title/series/issueNumber/
/// comicPageCount (written by the indexer itself, not by a manual PATCH)
/// are byte-for-byte identical.
#[tokio::test]
async fn given_tagged_cbz_file_when_indexed_via_http_and_ffi_then_extracted_metadata_matches() {
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    let http_comic = http_lib.path().join("issue1.cbz");
    write_minimal_cbz(&http_comic, "Parity Title", "Parity Series", "3", 24);

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
    wait_for_http_comic_extraction(&http_pool, "issue1.cbz").await;

    let (http_uuid,): (String,) = sqlx::query_as("SELECT uuid FROM files WHERE name = ?")
        .bind("issue1.cbz")
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
    let ffi_comic = ffi_lib.path().join("issue1.cbz");
    write_minimal_cbz(&ffi_comic, "Parity Title", "Parity Series", "3", 24);
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

        // Poll the FFI leg's own sqlite file directly for all four
        // extraction writes (title, series, issue_number, page_count) —
        // not just file-row existence, and not just the first of the
        // writes the indexer commits across its separate transactions.
        type FfiComicExtractionRow = (String, Option<String>, Option<String>, Option<i64>, Option<i64>);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ffi_uuid: String = rt.block_on(async {
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect(&format!("sqlite://{ffi_db_for_poll}?mode=rw"))
                .await
                .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let row: Option<FfiComicExtractionRow> = sqlx::query_as(
                    "SELECT files.uuid, comic_books.title, comic_books.series, \
                     comic_books.issue_number, comic_books.page_count \
                     FROM comic_books \
                     JOIN files ON files.id = comic_books.file_id \
                     WHERE files.name = ?",
                )
                .bind("issue1.cbz")
                .fetch_optional(&pool)
                .await
                .unwrap();
                if let Some((uuid, Some(_), Some(_), Some(_), Some(_))) = row {
                    return uuid;
                }
                if std::time::Instant::now() > deadline {
                    panic!("ffi never wrote extracted comic metadata");
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
    assert_eq!(http_body["comicPageCount"], ffi_body["comicPageCount"]);
    assert_eq!(http_body["metadata"], ffi_body["metadata"]);
    assert_eq!(http_body["comicPageCount"], 24);
    assert_eq!(http_body["metadata"]["title"], "Parity Title");
    assert_eq!(http_body["metadata"]["series"], "Parity Series");
    assert_eq!(http_body["metadata"]["issueNumber"], 3);
}
```

Before finalizing: `FileView` carries `#[serde(rename_all = "camelCase")]`, so `comic_page_count` serializes as `comicPageCount`. Confirm this yourself by reading `crates/alexandria-core/src/catalog/model.rs`'s current `FileView` struct definition (from Task 5) rather than assuming this plan text is still accurate. Also confirm `SubtypeMetadata::Comic`'s `issue_number` field carries `#[serde(rename = "issueNumber")]` (it already does, unchanged by this slice) — the assertion above uses `issueNumber`, matching that existing rename.

- [ ] **Step 3: Run test to verify it passes**

Run: `cargo test -p alexandria-ffi --test parity given_tagged_cbz_file_when_indexed_via_http_and_ffi -- --nocapture`
Expected: `test result: ok. 1 passed`. As with all four prior slices' equivalent task, this only exercises code paths built in Tasks 1–7, so there's no meaningful "write it failing first" step — the assertions either hold given the prior tasks' implementation, or reveal a real bug in it.

- [ ] **Step 4: Run the full parity suite to confirm no regression**

Run: `cargo test -p alexandria-ffi --test parity`
Expected: every test in the file passes.

- [ ] **Step 5: `fmt` + `clippy`**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings` (exact literal commands). Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/alexandria-ffi/Cargo.toml crates/alexandria-ffi/tests/parity.rs Cargo.lock
git commit -m "test: add HTTP/FFI parity coverage for extracted comic metadata"
```

---

### Task 9: Update SRD and close out issue #44 documentation debt

**Files:**
- Modify: `docs/requirements/System Requirements Document.md`

**Interfaces:** none — documentation only.

The final whole-branch review of the video slice flagged that the SRD's `VideoFile` row never got updated with `durationSeconds` when that field shipped, and that this doc-sync gap has existed since the audio slice (no single task in any slice's plan owns updating the SRD's representative-field tables). This is the fifth and final slice of issue #44, so it's the natural place to close that gap for every slice at once rather than letting it compound further.

- [ ] **Step 1: Find and update the representative-fields table**

Search `docs/requirements/System Requirements Document.md` for the section listing representative subtype fields per file type (look for `VideoFile`, `DocumentFile`, `AudioFile`, `ImageFile`, `ComicFile` or similar row labels — read the actual current section yourself, since its exact heading and table format may not match what's guessed here).

Update it so every file type's row reflects what's actually extracted as of this branch:
- `AudioFile`: unchanged (already accurate since the audio slice).
- `ImageFile`: unchanged (already accurate since the image slice).
- `DocumentFile`: unchanged (already accurate since the document slice).
- `VideoFile`: add `durationSeconds` if missing (the video slice's final review found this gap).
- `ComicFile`: add `pageCount` (or `comicPageCount`, matching whatever field name convention the table already uses for cross-referencing the API field — check how the `DocumentFile`/`VideoFile` rows reference their own extracted-but-not-owner-editable fields for the established convention to follow).

- [ ] **Step 2: Commit**

```bash
git add "docs/requirements/System Requirements Document.md"
git commit -m "docs: update SRD representative fields for video/comic extraction"
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
git push -u origin feature/comic-metadata-extraction
```

```bash
gh pr create --title "feat: extract comic metadata during indexing (issue #44 comic slice)" --body "$(cat <<'EOF'
## Summary
- Implements the comic slice of issue #44 — the fifth and final slice: indexing now reads `ComicInfo.xml` (when present) and counts archive image entries for `.cbz` files, pre-populating title, series, issue number, and page count instead of leaving every field for the owner to enter manually.
- Format scope: **CBZ only** — `.cbr` (RAR) has no viable pure-Rust reader and stays unextracted, the same graceful degradation document's mobi/azw/azw3 established.
- `title`/`series`/`issue_number` come from `ComicInfo.xml` (the de-facto ComicRack/ComicVine standard) when the archive has one; `page_count` is always computed by counting image entries, independent of whether `ComicInfo.xml` exists at all — needing the same narrow new repository method + `FileView` field pattern image/document/video all established (no new migration needed, `comic_books.page_count` already existed).
- `FileView`'s new field is named `comicPageCount` (not `pageCount`) to avoid colliding with the document slice's existing `pageCount` field on the same struct.
- Extraction runs once, at first index only; `refresh.rs` is untouched. Extraction failure never fails the indexing run.
- **This closes issue #44** — audio, image, document, video, and comic all now have best-effort metadata extraction at first index.

See \`docs/superpowers/specs/2026-08-10-comic-metadata-extraction-design.md\` for the full design.

Closes #44.

## Test plan
- [x] \`cargo test --workspace\` — all green
- [x] \`cargo fmt --all\` / \`cargo clippy --workspace --all-targets -- -D warnings\`
- [x] Unit tests: \`CbzComicMetadataReader\` against real generated CBZ fixtures (with and without \`ComicInfo.xml\`, plus a non-integer issue number case), repository \`set_comic_page_count\`/\`find_comic_page_count\`, \`BrowseFilesHandler::get_by_uuid\` comic_page_count wiring, \`IndexHandler\` against \`FakeComicMetadataReader\` (full tags/page-count-only/issue-number-only/unopenable/non-comic, with a call-count assertion proving the reader is never consulted for non-comic files)
- [x] HTTP/FFI parity test: index a real tagged CBZ through both surfaces, assert extracted page count + metadata match (race-free — both legs poll on all extraction writes landing, not just file-row existence)

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 3: Wait for CI, then merge**

Run: `gh pr checks <PR number> --watch`
Expected: all checks pass. If CI fails and the failure doesn't obviously look like a real code defect (e.g. a runner-acquisition or infrastructure error rather than a compile/test failure), retry the run once (`gh run rerun <run-id>`) before assuming it's a code problem — this project's CI has hit transient infrastructure flakiness on prior slices' PRs.

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

- [ ] **Step 5: Close issue #44**

```bash
gh issue close 44 --comment "All five file-type slices (audio, image, document, video, comic) have shipped. Best-effort metadata extraction now runs at first index for every supported file type."
```
