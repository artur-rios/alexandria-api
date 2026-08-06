# Document Metadata Extraction (3rd slice of issue #44) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Read embedded PDF/EPUB metadata at index time and pre-populate a document file's title, author, year, format kind, and (PDF only) page count, instead of leaving every field for the owner to type in via UC-04.

**Architecture:** A new `DocumentMetadataReader` trait port (mirroring `AudioMetadataReader`/`ImageMetadataReader` from the two already-shipped slices) becomes a 7th generic collaborator on `IndexHandler`. Unlike audio/image (one library covers every extension of that type), document needs two different parsers behind one port — `lopdf` for `.pdf`, the `epub` crate for `.epub` — dispatched internally by file extension; `.mobi`/`.azw`/`.azw3` always yield nothing. `page_count` lives outside `SubtypeMetadata::Document` (same situation as image's `width`/`height`), so this slice adds one narrow new repository write method, one narrow new read method, and a third `FileView` field.

**Tech Stack:** Rust, `lopdf` (new dependency, real PDF reader/writer), `epub` (new dependency, real EPUB reader), `zip` (new dev-dependency, to build a real EPUB fixture at test time — the `epub` crate is read-only).

## Global Constraints

- Spec doc: `docs/superpowers/specs/2026-08-06-document-metadata-extraction-design.md` — read it first if anything below is ambiguous.
- Format scope: **PDF and EPUB only**. `.mobi`/`.azw`/`.azw3` always yield `None` from the reader — no attempt to parse them.
- `format_kind` is set unconditionally the moment either branch (PDF/EPUB) is entered — `Book` for PDF, `Ebook` for EPUB — independent of whether title/author/year were found.
- `page_count` only ever comes from the PDF branch; the EPUB branch always leaves it `None`.
- Extraction runs **once, at first index only**. Never touch `refresh.rs`.
- Extraction failure (unsupported extension, corrupt PDF, malformed EPUB zip, missing metadata) is **never** a run failure: not counted in `IndexOutcome::failed`, logged at `debug` at most.
- The `page_count` write (`set_document_page_count`) and the title/author/year/format_kind write (`update_metadata`) are **independent** — a failure in one must not block or be conflated with the other, and neither fails indexing.
- `lopdf`, `epub`, and `zip`'s exact method/type names are best-effort based on their documented APIs at the time of writing; if a name has moved in the resolved version, fix it against `cargo doc -p <crate> --open` — this is the same situation two already-shipped slices handled successfully with `lofty`, `kamadak-exif`, and `little_exif`.
- Every new/changed Rust file must pass `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` before its task is done — report the **exact literal commands**, not narrower or `--check`-only variants. This has been flagged repeatedly across both prior slices; get it right the first time.
- Branch: `feature/document-metadata-extraction` off `main`. One PR at the end of Task 9, following this repo's established branch → PR → CI → squash-merge cycle.

---

### Task 1: `DocumentTags` type and `DocumentMetadataReader` trait

**Files:**
- Create: `crates/alexandria-core/src/catalog/document_tags.rs`
- Modify: `crates/alexandria-core/src/catalog/mod.rs`

**Interfaces:**
- Produces: `pub struct DocumentTags { pub title: Option<String>, pub author: Option<String>, pub year: Option<i64>, pub format_kind: Option<FormatKind>, pub page_count: Option<i64> }`
- Produces: `#[allow(async_fn_in_trait)] pub trait DocumentMetadataReader: Send + Sync { async fn read(&self, path: &str) -> Option<DocumentTags>; }`

Pure logic, no I/O, no new dependency yet — mirrors both prior slices' Task 1 exactly in shape.

- [ ] **Step 1: Write the file**

Create `crates/alexandria-core/src/catalog/document_tags.rs`:

```rust
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
```

Add the module to `crates/alexandria-core/src/catalog/mod.rs` — it currently
reads:

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

Change to (inserting `document_tags` alphabetically between `commands` and `fs`):

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

- [ ] **Step 2: Confirm it compiles**

Run: `cargo build -p alexandria-core`
Expected: builds cleanly. No tests in this task — like `ImageTags`,
`DocumentTags` has no combinator method to test in isolation (its writes
are dispatched directly by `IndexHandler` in Task 6).

- [ ] **Step 3: `fmt` + `clippy`**

Run: `cargo fmt --all && cargo clippy -p alexandria-core --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 4: Commit**

```bash
git add crates/alexandria-core/src/catalog/document_tags.rs crates/alexandria-core/src/catalog/mod.rs
git commit -m "feat: add DocumentTags and DocumentMetadataReader port"
```

---

### Task 2: `PdfEpubMetadataReader` (real implementation)

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Modify: `crates/alexandria-core/Cargo.toml`
- Modify: `crates/alexandria-core/src/catalog/document_tags.rs`

**Interfaces:**
- Consumes: `DocumentTags`, `DocumentMetadataReader` from Task 1.
- Produces: `#[derive(Debug, Default, Clone, Copy)] pub struct PdfEpubMetadataReader;` implementing `DocumentMetadataReader`.

This is the riskiest task in the plan, for two independent reasons:
1. `lopdf` (the PDF library) can both read and write, so its own fixture can
   be built and read back with the same crate — but constructing a minimal
   valid PDF from scratch touches several interdependent PDF objects
   (catalog, page tree, page, content stream), more moving parts than the
   WAV/JPEG fixtures the two prior slices generated.
2. `epub` (the EPUB library) is **read-only**, like `kamadak-exif` was for
   the image slice. Unlike that slice (where `little_exif` filled the write
   gap), no EPUB-writing crate is needed here — a `.epub` file is just a
   zip with a specific internal structure, so the `zip` crate (dev-only) is
   enough to hand-construct one.

- [ ] **Step 1: Add the dependencies**

In `Cargo.toml` (workspace root), `[workspace.dependencies]` — insert
`epub` alphabetically between `chrono` and `jsonwebtoken`, and `lopdf`
alphabetically between `lofty` and `reqwest`:

```toml
chrono = { version = "0.4", features = ["serde"] }
epub = "2"
jsonwebtoken = "9"
kamadak-exif = "0.5"
lofty = "0.22"
lopdf = "0.34"
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
```

In the same file's dev-only section (currently `image`, `little_exif`,
`tempfile`), add `zip` at the end (alphabetically last):

```toml
# dev-only
image = { version = "0.25", default-features = false, features = ["jpeg"] }
little_exif = "0.6"
tempfile = "3"
zip = "2"
```

In `crates/alexandria-core/Cargo.toml`'s `[dependencies]` — insert `epub`
alphabetically after `chrono`, before `jsonwebtoken`, and `lopdf`
alphabetically after `lofty`, before `reqwest`:

```toml
chrono.workspace = true
epub.workspace = true
jsonwebtoken.workspace = true
kamadak-exif.workspace = true
lofty.workspace = true
lopdf.workspace = true
reqwest.workspace = true
```

In `crates/alexandria-core/Cargo.toml`'s `[dev-dependencies]` (currently
`image`, `little_exif`, `tempfile`, `tokio`, `toml`) — add `zip` at the end:

```toml
[dev-dependencies]
image.workspace = true
little_exif.workspace = true
tempfile.workspace = true
tokio.workspace = true
toml.workspace = true
zip.workspace = true
```

Run: `cargo build -p alexandria-core --all-targets`
Expected: builds successfully, `Cargo.lock` updates. If `epub`, `lopdf`, or
`zip` don't resolve exactly as pinned, adjust the version to the latest
available for that major line and note the change in your report — this is
a normal dependency-resolution step, not a plan defect.

- [ ] **Step 2: Write the failing test**

Append to `crates/alexandria-core/src/catalog/document_tags.rs`, inside a
new `#[cfg(test)] mod tests` block at the end of the file:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal valid PDF with `lopdf` itself: a catalog, a single
    /// empty page, and an `/Info` dictionary carrying Title/Author. This
    /// is a real, valid PDF file — not hand-crafted bytes.
    fn write_minimal_pdf(path: &std::path::Path, title: &str, author: &str) {
        use lopdf::{dictionary, Document, Object};

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();

        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let content = lopdf::content::Content {
            operations: vec![],
        };
        let content_id = doc.add_object(lopdf::Stream::new(
            dictionary! {},
            content.encode().expect("encode content"),
        ));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);

        let info_id = doc.add_object(dictionary! {
            "Title" => Object::string_literal(title),
            "Author" => Object::string_literal(author),
        });
        doc.trailer.set("Info", info_id);

        doc.save(path).expect("save pdf");
    }

    /// Build a minimal valid EPUB by hand-constructing the zip structure
    /// (mimetype, container.xml, a content.opf with Dublin Core metadata,
    /// one empty chapter, and a stub NCX) using the `zip` crate. `epub` is
    /// read-only, so this is the fixture-generation path — a real zip with
    /// real EPUB structure, not a checked-in binary.
    fn write_minimal_epub(path: &std::path::Path, title: &str, author: &str) {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let file = std::fs::File::create(path).expect("create epub file");
        let mut zip = zip::ZipWriter::new(file);

        zip.start_file(
            "mimetype",
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
        )
        .expect("start mimetype");
        zip.write_all(b"application/epub+zip").expect("write mimetype");

        zip.start_file("META-INF/container.xml", SimpleFileOptions::default())
            .expect("start container.xml");
        zip.write_all(
            br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#,
        )
        .expect("write container.xml");

        zip.start_file("OEBPS/content.opf", SimpleFileOptions::default())
            .expect("start content.opf");
        let opf = format!(
            r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" unique-identifier="BookId" version="2.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>{title}</dc:title>
    <dc:creator>{author}</dc:creator>
    <dc:identifier id="BookId">urn:uuid:test-fixture</dc:identifier>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="chap1" href="chap1.xhtml" media-type="application/xhtml+xml"/>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
  </manifest>
  <spine toc="ncx">
    <itemref idref="chap1"/>
  </spine>
</package>"#
        );
        zip.write_all(opf.as_bytes()).expect("write content.opf");

        zip.start_file("OEBPS/chap1.xhtml", SimpleFileOptions::default())
            .expect("start chap1.xhtml");
        zip.write_all(b"<html><body><p>Test</p></body></html>")
            .expect("write chap1.xhtml");

        zip.start_file("OEBPS/toc.ncx", SimpleFileOptions::default())
            .expect("start toc.ncx");
        zip.write_all(
            br#"<?xml version="1.0"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <head></head>
  <docTitle><text>Test</text></docTitle>
  <navMap></navMap>
</ncx>"#,
        )
        .expect("write toc.ncx");

        zip.finish().expect("finish epub zip");
    }

    #[tokio::test]
    async fn given_tagged_pdf_when_read_then_title_author_and_page_count_extracted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tagged.pdf");
        write_minimal_pdf(&path, "Test Title", "Test Author");

        let reader = PdfEpubMetadataReader;
        let tags = reader
            .read(path.to_str().unwrap())
            .await
            .expect("tags extracted");

        assert_eq!(tags.title.as_deref(), Some("Test Title"));
        assert_eq!(tags.author.as_deref(), Some("Test Author"));
        assert_eq!(tags.page_count, Some(1));
        assert_eq!(tags.format_kind, Some(FormatKind::Book));
    }

    #[tokio::test]
    async fn given_tagged_epub_when_read_then_title_and_author_extracted_no_page_count() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tagged.epub");
        write_minimal_epub(&path, "Test Title", "Test Author");

        let reader = PdfEpubMetadataReader;
        let tags = reader
            .read(path.to_str().unwrap())
            .await
            .expect("tags extracted");

        assert_eq!(tags.title.as_deref(), Some("Test Title"));
        assert_eq!(tags.author.as_deref(), Some("Test Author"));
        assert_eq!(tags.page_count, None, "EPUB never sets page_count");
        assert_eq!(tags.format_kind, Some(FormatKind::Ebook));
    }

    #[tokio::test]
    async fn given_unsupported_extension_when_read_then_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("book.mobi");
        std::fs::write(&path, b"not a real mobi file").expect("write stub");

        let reader = PdfEpubMetadataReader;
        let tags = reader.read(path.to_str().unwrap()).await;

        assert!(tags.is_none(), ".mobi is not attempted at all");
    }

    #[tokio::test]
    async fn given_missing_file_when_read_then_none_not_panic() {
        let reader = PdfEpubMetadataReader;

        let tags = reader.read("/no/such/file.pdf").await;

        assert!(tags.is_none());
    }
}
```

If `lopdf`'s `dictionary!` macro, `Object::string_literal`, `Document::new_object_id`/`add_object`/`save`, or `lopdf::content::Content` don't match the resolved version's actual API, or `zip`'s `SimpleFileOptions`/`ZipWriter`/`CompressionMethod` differ, adapt via `cargo doc -p lopdf --open` / `cargo doc -p zip --open` — keep the same intent (a minimal valid PDF with one page and an `/Info` dict; a minimal valid EPUB zip with the standard mimetype/container/OPF structure).

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p alexandria-core --lib catalog::document_tags`
Expected: fails to compile — `PdfEpubMetadataReader` does not exist yet.

- [ ] **Step 4: Implement `PdfEpubMetadataReader`**

Add above the `#[cfg(test)]` block in `document_tags.rs`:

```rust
/// Real document reader covering PDF (via `lopdf`) and EPUB (via the
/// `epub` crate) — 2 of the 5 extensions `classify_by_extension` maps to
/// `FileType::Document`. `.mobi`/`.azw`/`.azw3` (proprietary Amazon Kindle
/// formats) have no workable pure-Rust library and always yield `None` —
/// the same graceful degradation the audio and image slices established
/// for `.wma` and non-EXIF image formats.
#[derive(Debug, Default, Clone, Copy)]
pub struct PdfEpubMetadataReader;

impl PdfEpubMetadataReader {
    fn read_pdf(path: &str) -> Option<DocumentTags> {
        let doc = match lopdf::Document::load(path) {
            Ok(d) => d,
            Err(err) => {
                tracing::debug!(path, error = %err, "could not parse pdf");
                return None;
            }
        };

        let page_count = doc.get_pages().len() as i64;

        let info_dict = doc
            .trailer
            .get(b"Info")
            .ok()
            .and_then(|obj| obj.as_reference().ok())
            .and_then(|id| doc.get_dictionary(id).ok());

        let title = info_dict
            .and_then(|d| d.get(b"Title").ok())
            .and_then(|v| v.as_str().ok())
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let author = info_dict
            .and_then(|d| d.get(b"Author").ok())
            .and_then(|v| v.as_str().ok())
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        Some(DocumentTags {
            title,
            author,
            year: None,
            format_kind: Some(FormatKind::Book),
            page_count: Some(page_count),
        })
    }

    fn read_epub(path: &str) -> Option<DocumentTags> {
        let doc = match epub::doc::EpubDoc::new(path) {
            Ok(d) => d,
            Err(err) => {
                tracing::debug!(path, error = %err, "could not parse epub");
                return None;
            }
        };

        let title = doc
            .mdata("title")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let author = doc
            .mdata("creator")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        Some(DocumentTags {
            title,
            author,
            year: None,
            format_kind: Some(FormatKind::Ebook),
            page_count: None,
        })
    }
}

impl DocumentMetadataReader for PdfEpubMetadataReader {
    async fn read(&self, path: &str) -> Option<DocumentTags> {
        let lower = path.to_ascii_lowercase();
        if lower.ends_with(".pdf") {
            Self::read_pdf(path)
        } else if lower.ends_with(".epub") {
            Self::read_epub(path)
        } else {
            None
        }
    }
}
```

If `lopdf::Document::load`/`get_pages`/`trailer`/`get_dictionary`, or `epub::doc::EpubDoc::new`/`mdata` don't match the resolved versions' actual APIs, adapt via `cargo doc` as above. Keep the same intent: PDF path count via the page tree, `/Info` Title/Author as trimmed non-empty strings; EPUB path via Dublin Core `title`/`creator` metadata, same trimming.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p alexandria-core --lib catalog::document_tags`
Expected: `test result: ok. 4 passed; 0 failed`

- [ ] **Step 6: `fmt` + `clippy`**

Run: `cargo fmt --all && cargo clippy -p alexandria-core --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock crates/alexandria-core/Cargo.toml crates/alexandria-core/src/catalog/document_tags.rs
git commit -m "feat: implement PdfEpubMetadataReader"
```

---

### Task 3: `FakeDocumentMetadataReader` test double

**Files:**
- Modify: `crates/alexandria-core/tests/common/mod.rs`

**Interfaces:**
- Consumes: `DocumentMetadataReader`, `DocumentTags` from `alexandria_core::catalog::document_tags`.
- Produces: `FakeDocumentMetadataReader::new()`, `.seed(path: &str, tags: DocumentTags)`, `.call_count()`, implementing `DocumentMetadataReader`.

Mirrors `FakeAudioMetadataReader`/`FakeImageMetadataReader` (already in this file) exactly, including the call-count pattern.

- [ ] **Step 1: Add the fake**

Add this import near the top of `crates/alexandria-core/tests/common/mod.rs`, alongside the existing `alexandria_core::catalog::audio_tags::...` and `alexandria_core::catalog::image_tags::...` imports:

```rust
use alexandria_core::catalog::audio_tags::{AudioMetadataReader, AudioTags};
use alexandria_core::catalog::document_tags::{DocumentMetadataReader, DocumentTags};
use alexandria_core::catalog::image_tags::{ImageMetadataReader, ImageTags};
```

Append this new fake at the end of the file, after `FakeImageMetadataReader`'s `impl ImageMetadataReader for FakeImageMetadataReader` block:

```rust
/// In-memory document reader (issue #44 document slice). `read()` answers
/// `None` for any path with no seeded tags, mirroring "unsupported
/// extension / no metadata / couldn't parse" — the same outcome
/// `PdfEpubMetadataReader` produces for those cases. Also counts calls, so
/// a test can assert the reader was never consulted at all (e.g. for a
/// non-document file).
#[derive(Debug, Default, Clone)]
pub struct FakeDocumentMetadataReader {
    tags: Arc<Mutex<HashMap<String, DocumentTags>>>,
    call_count: Arc<Mutex<usize>>,
}

impl FakeDocumentMetadataReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the tags `read()` returns for `path`.
    pub fn seed(&self, path: &str, tags: DocumentTags) -> &Self {
        self.tags.lock().unwrap().insert(path.to_string(), tags);
        self
    }

    /// How many times `read()` has been called.
    pub fn call_count(&self) -> usize {
        *self.call_count.lock().unwrap()
    }
}

impl DocumentMetadataReader for FakeDocumentMetadataReader {
    async fn read(&self, path: &str) -> Option<DocumentTags> {
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
git commit -m "test: add FakeDocumentMetadataReader test double"
```

---

### Task 4: Repository methods `set_document_page_count` and `find_document_page_count`

**Files:**
- Modify: `crates/alexandria-core/src/catalog/repos.rs`
- Modify: `crates/alexandria-core/tests/common/mod.rs`

**Interfaces:**
- Produces (on `CatalogRepository` trait and its `SqliteCatalogRepository`/`FakeCatalogRepository` implementations):
  - `async fn set_document_page_count(&self, uuid: Uuid, page_count: i64) -> Result<(), DomainError>`
  - `async fn find_document_page_count(&self, uuid: Uuid) -> Result<Option<i64>, DomainError>`
- Produces on `FakeCatalogRepository`: `pub fn document_page_count_for(&self, uuid: Uuid) -> Option<i64>` (test inspector, mirrors the existing `dimensions_for`).

Mirrors the image slice's `set_image_dimensions`/`find_image_dimensions` exactly in shape (this codebase's own precedent for "data outside `SubtypeMetadata`").

- [ ] **Step 1: Add the trait methods**

In `crates/alexandria-core/src/catalog/repos.rs`, in the `CatalogRepository` trait, add these two methods right after the existing `find_image_dimensions` method:

```rust
    /// Write a document file's page count (issue #44 document slice).
    /// Unlike `update_metadata`, this touches `documents.page_count`
    /// directly — `SubtypeMetadata::Document` deliberately excludes it
    /// because it is not owner-editable (UC-04). Returns `NotFound` when
    /// no file row carries the UUID, `InvalidInput` when the file is not a
    /// document.
    async fn set_document_page_count(
        &self,
        uuid: Uuid,
        page_count: i64,
    ) -> Result<(), DomainError>;

    /// Read a document file's page count, if set (issue #44 document
    /// slice). `None` when the file doesn't exist, isn't a document, or
    /// the column is still `NULL` (extraction never ran, or the file was
    /// EPUB — EPUB never sets this).
    async fn find_document_page_count(&self, uuid: Uuid) -> Result<Option<i64>, DomainError>;
```

- [ ] **Step 2: Add the fakes**

In `crates/alexandria-core/tests/common/mod.rs`, add a new field to `FakeCatalogRepository`'s struct definition — it currently ends with:

```rust
    /// Dimensions last written for `uuid` via `set_image_dimensions`
    /// (issue #44 image slice).
    dimensions: Arc<Mutex<HashMap<Uuid, (i64, i64)>>>,
}
```

Change to:

```rust
    /// Dimensions last written for `uuid` via `set_image_dimensions`
    /// (issue #44 image slice).
    dimensions: Arc<Mutex<HashMap<Uuid, (i64, i64)>>>,
    /// Page count last written for `uuid` via `set_document_page_count`
    /// (issue #44 document slice).
    document_page_counts: Arc<Mutex<HashMap<Uuid, i64>>>,
}
```

Add an inspector method in `impl FakeCatalogRepository`, right after the existing `dimensions_for` method:

```rust
    /// Page count last written for `uuid` via `set_document_page_count`.
    /// `None` means no call has landed for that file yet.
    pub fn document_page_count_for(&self, uuid: Uuid) -> Option<i64> {
        self.document_page_counts.lock().unwrap().get(&uuid).copied()
    }
```

Add the two trait method implementations in `impl CatalogRepository for FakeCatalogRepository`, right after the existing `find_image_dimensions` implementation:

```rust
    async fn set_document_page_count(
        &self,
        uuid: Uuid,
        page_count: i64,
    ) -> Result<(), DomainError> {
        let files = self.files.lock().unwrap();
        let file = files
            .values()
            .find(|f| f.uuid == uuid)
            .ok_or(DomainError::NotFound)?;
        if file.file_type != alexandria_core::catalog::model::FileType::Document {
            return Err(DomainError::InvalidInput("file is not a document".into()));
        }
        drop(files);
        self.document_page_counts
            .lock()
            .unwrap()
            .insert(uuid, page_count);
        Ok(())
    }

    async fn find_document_page_count(&self, uuid: Uuid) -> Result<Option<i64>, DomainError> {
        let files = self.files.lock().unwrap();
        let file = match files.values().find(|f| f.uuid == uuid) {
            Some(f) => f,
            None => return Ok(None),
        };
        if file.file_type != alexandria_core::catalog::model::FileType::Document {
            return Ok(None);
        }
        drop(files);
        Ok(self.document_page_counts.lock().unwrap().get(&uuid).copied())
    }
```

- [ ] **Step 3: Confirm the fakes compile**

Run: `cargo test -p alexandria-core --test catalog -- --list`
Expected: compiles cleanly. `cargo build -p alexandria-core` will still fail at this point — the trait now has two new required methods and `SqliteCatalogRepository` doesn't implement them yet. That's expected; the next step fixes it.

- [ ] **Step 4: Implement the real Sqlite methods**

In `crates/alexandria-core/src/catalog/repos.rs`, in `impl CatalogRepository for SqliteCatalogRepository`, add these two methods right after the existing `find_image_dimensions` implementation:

```rust
    async fn set_document_page_count(
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
        if actual_type != FileType::Document {
            return Err(DomainError::InvalidInput("file is not a document".into()));
        }

        let affected = sqlx::query("UPDATE documents SET page_count = ? WHERE file_id = ?")
            .bind(page_count)
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();

        if affected == 0 {
            return Err(DomainError::internal(format!(
                "subtype row missing for file {uuid} (document)"
            )));
        }

        tx.commit().await?;
        Ok(())
    }

    async fn find_document_page_count(&self, uuid: Uuid) -> Result<Option<i64>, DomainError> {
        let row: Option<(i64, String)> =
            sqlx::query_as("SELECT id, type FROM files WHERE uuid = ?")
                .bind(uuid.to_string())
                .fetch_optional(&self.pool)
                .await?;
        let (id, type_str) = match row {
            Some(r) => r,
            None => return Ok(None),
        };
        if parse_type_str(&type_str)? != FileType::Document {
            return Ok(None);
        }

        let row: Option<(Option<i64>,)> =
            sqlx::query_as("SELECT page_count FROM documents WHERE file_id = ?")
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
git commit -m "feat: add set_document_page_count and find_document_page_count to CatalogRepository"
```

---

### Task 5: `FileView` page_count and `BrowseFilesHandler::get_by_uuid` wiring

**Files:**
- Modify: `crates/alexandria-core/src/catalog/model.rs`
- Modify: `crates/alexandria-core/src/catalog/queries/browse.rs`
- Test: `crates/alexandria-core/tests/catalog/browse.rs`

**Interfaces:**
- Consumes: `CatalogRepository::find_document_page_count` from Task 4.
- Produces: `FileView { file, metadata, width, height, page_count: Option<i64> }` (was `{ file, metadata, width, height }`).

This closes the read-path gap for document, exactly mirroring the image slice's Task 5. No HTTP or FFI code needs to change — both already serialize `FileView` generically.

- [ ] **Step 1: Write the failing test**

`crates/alexandria-core/tests/catalog/browse.rs` already imports `FormatKind`, `existing_file_with_hash`, `FakeCatalogRepository`, `FakeAuth`, `handler`, `TOKEN` (used by the existing image tests added in the image slice — search for `given_image_with_extracted_dimensions_when_get_by_uuid_then_width_and_height_present` for the exact pattern to follow). Add these 3 new tests near it, following that same pattern exactly:

```rust
#[tokio::test]
async fn given_document_with_extracted_page_count_when_get_by_uuid_then_page_count_present() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/book.pdf", "book", FileType::Document, "h");
    let uuid = file.uuid;
    repo.seed(file);
    repo.set_document_page_count(uuid, 42).await.unwrap();

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    assert_eq!(view.page_count, Some(42));
}

#[tokio::test]
async fn given_document_with_no_extracted_page_count_when_get_by_uuid_then_page_count_none() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/book.epub", "book", FileType::Document, "h");
    let uuid = file.uuid;
    repo.seed(file);

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    assert_eq!(view.page_count, None);
}

#[tokio::test]
async fn given_non_document_file_when_get_by_uuid_then_page_count_none() {
    let repo = FakeCatalogRepository::new();
    let file = existing_file_with_hash("/lib/song.mp3", "song", FileType::Audio, "h");
    let uuid = file.uuid;
    repo.seed(file);

    let h = handler(FakeAuth::Allowing, repo);
    let view = h.get_by_uuid(uuid, TOKEN).await.expect("get");

    assert_eq!(view.page_count, None);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p alexandria-core --test catalog -- browse::`
Expected: fails to compile — `FileView` has no `page_count` field yet, and `get_by_uuid`'s return doesn't set it.

- [ ] **Step 3: Add the field to `FileView`**

In `crates/alexandria-core/src/catalog/model.rs`, find:

```rust
#[derive(Debug, Clone, Serialize)]
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
}
```

Change to:

```rust
#[derive(Debug, Clone, Serialize)]
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
}
```

- [ ] **Step 4: Wire the read in `BrowseFilesHandler::get_by_uuid`**

In `crates/alexandria-core/src/catalog/queries/browse.rs`, find:

```rust
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
}
```

Change to:

```rust
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
git commit -m "feat: surface extracted document page count through FileView"
```

---

### Task 6: Wire `DocumentMetadataReader` into `IndexHandler`

**Files:**
- Modify: `crates/alexandria-core/src/catalog/commands/index.rs`
- Modify: `crates/alexandria-core/tests/catalog/index.rs`

**Interfaces:**
- Consumes: `DocumentMetadataReader`, `DocumentTags` from Task 1; `FakeDocumentMetadataReader` from Task 3; `CatalogRepository::set_document_page_count` from Task 4.
- Produces: `IndexHandler<A, R, F, C, M, N, O>` (was `<A, R, F, C, M, N>`) — `O: DocumentMetadataReader` is the new 7th parameter, with `pub fn new(auth: A, repo: R, fs: F, clock: C, audio_tags: M, image_tags: N, document_tags: O) -> Self` (was 6 params).

Mirrors the image slice's Task 6 exactly in shape: widening `IndexHandler`'s constructor arity means every existing call site needs the new argument — this file's own tests, plus (in Task 7) `services.rs`. This task deliberately leaves `services.rs` broken; fixing it is Task 7's job.

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
use alexandria_core::catalog::fs::Filesystem;
use alexandria_core::catalog::image_tags::{ImageMetadataReader, ImageTags};
use alexandria_core::catalog::model::{FileType, SubtypeMetadata};
use alexandria_core::catalog::repos::CatalogRepository;
use alexandria_core::errors::DomainError;

use crate::common::{
    existing_file, fixed_clock, now, FakeAudioMetadataReader, FakeAuth, FakeCatalogRepository,
    FakeFilesystem, FakeImageMetadataReader,
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
use alexandria_core::errors::DomainError;

use crate::common::{
    existing_file, fixed_clock, now, FakeAudioMetadataReader, FakeAuth, FakeCatalogRepository,
    FakeDocumentMetadataReader, FakeFilesystem, FakeImageMetadataReader,
};
```

**1b.** Change the `handler` helper function. It currently reads:

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

Change to:

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

**1c.** Every existing call to `handler(...)` in this file needs a 7th argument, `FakeDocumentMetadataReader::new()`. There are 16 call sites, falling into exactly two literal shapes — find every occurrence of each and replace **all** occurrences (a single find-all-replace per shape covers every one):

**Shape 1** (11 occurrences) — the call's last argument before the closing `);` is the literal `FakeImageMetadataReader::new()`. Change every occurrence of:
```
        FakeImageMetadataReader::new(),
    );
```
to:
```
        FakeImageMetadataReader::new(),
        FakeDocumentMetadataReader::new(),
    );
```

**Shape 2** (5 occurrences) — the call's last argument before the closing `);` is a named `image_tags` variable (these are the tests that seed or inspect the image reader, so they build it as a variable earlier in the test body). Change every occurrence of:
```
        image_tags,
    );
```
to:
```
        image_tags,
        FakeDocumentMetadataReader::new(),
    );
```

After both replacements, verify no call site was missed:

Run: `grep -c "handler(" crates/alexandria-core/tests/catalog/index.rs` — note the count (call sites; unchanged by this step, since you're adding arguments, not new calls).
Run: `grep -c "FakeDocumentMetadataReader::new()" crates/alexandria-core/tests/catalog/index.rs` — expect this to equal the `handler(` count from above (every pre-existing call site now has exactly one `FakeDocumentMetadataReader::new()`, whether inline or — after Step 1d adds new document-specific tests — as a named variable for those new tests specifically).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p alexandria-core --test catalog -- index::`
Expected: fails to compile — constructor arity mismatch, `document_tags` field doesn't exist on `IndexHandler` yet.

- [ ] **Step 3: Implement the change in `index.rs`**

In `crates/alexandria-core/src/catalog/commands/index.rs`, make these exact edits.

Add to the imports:

```rust
use crate::catalog::audio_tags::AudioMetadataReader;
use crate::catalog::document_tags::DocumentMetadataReader;
use crate::catalog::image_tags::ImageMetadataReader;
```

Change the struct + constructor from:

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

to:

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

Add a new `FileType::Document` branch at the end of `index_entry`, right after the existing `FileType::Image` branch and before the final `Ok(true)`:

```rust
        // Best-effort document metadata prefill (issue #44 document
        // slice). Two independent writes: page count (outside
        // SubtypeMetadata, via set_document_page_count — PDF only, EPUB
        // never sets it) and title/author/year/format_kind (via the
        // shared update_metadata). Neither write's failure blocks the
        // other or fails indexing.
        if file_type == FileType::Document {
            if let Some(tags) = self.document_tags.read(&entry.path).await {
                if let Some(page_count) = tags.page_count {
                    if let Err(err) = self
                        .repo
                        .set_document_page_count(file.uuid, page_count)
                        .await
                    {
                        tracing::warn!(
                            path = %entry.path,
                            error = %err,
                            "indexed but failed to write extracted document page count"
                        );
                    }
                }
                if tags.title.is_some()
                    || tags.author.is_some()
                    || tags.year.is_some()
                    || tags.format_kind.is_some()
                {
                    let metadata = crate::catalog::model::SubtypeMetadata::Document {
                        title: tags.title,
                        author: tags.author,
                        year: tags.year,
                        format_kind: tags.format_kind,
                    };
                    if let Err(err) = self.repo.update_metadata(file.uuid, &metadata).await {
                        tracing::warn!(
                            path = %entry.path,
                            error = %err,
                            "indexed but failed to write extracted document metadata"
                        );
                    }
                }
            }
        }
        Ok(true)
    }
}
```

(This replaces the file's existing final `Ok(true)\n    }\n}` — the new branch goes immediately before that line, after the existing `FileType::Image` block's closing `}`.)

**1d.** Add these new tests at the end of `crates/alexandria-core/tests/catalog/index.rs` (after the last existing test):

```rust
#[tokio::test]
async fn given_tagged_pdf_when_execute_then_page_count_and_metadata_written() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.pdf", "a.pdf", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let document_tags = FakeDocumentMetadataReader::new();
    document_tags.seed(
        "/library/a.pdf",
        DocumentTags {
            title: Some("A Book".to_string()),
            author: Some("An Author".to_string()),
            year: None,
            format_kind: Some(FormatKind::Book),
            page_count: Some(42),
        },
    );
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
        FakeImageMetadataReader::new(),
        document_tags,
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.pdf").expect("indexed");
    assert_eq!(repo_handle.document_page_count_for(a.uuid), Some(42));
    let metadata = repo_handle
        .metadata_for(a.uuid)
        .expect("metadata written from extracted tags");
    assert_eq!(
        metadata,
        SubtypeMetadata::Document {
            title: Some("A Book".to_string()),
            author: Some("An Author".to_string()),
            year: None,
            format_kind: Some(FormatKind::Book),
        }
    );
}

#[tokio::test]
async fn given_tagged_epub_when_execute_then_metadata_written_no_page_count() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.epub", "a.epub", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let document_tags = FakeDocumentMetadataReader::new();
    document_tags.seed(
        "/library/a.epub",
        DocumentTags {
            title: Some("An Ebook".to_string()),
            author: Some("An Author".to_string()),
            year: None,
            format_kind: Some(FormatKind::Ebook),
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
        document_tags,
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.epub").expect("indexed");
    assert_eq!(
        repo_handle.document_page_count_for(a.uuid),
        None,
        "EPUB never sets page_count"
    );
    let metadata = repo_handle
        .metadata_for(a.uuid)
        .expect("metadata written from extracted tags");
    assert_eq!(
        metadata,
        SubtypeMetadata::Document {
            title: Some("An Ebook".to_string()),
            author: Some("An Author".to_string()),
            year: None,
            format_kind: Some(FormatKind::Ebook),
        }
    );
}

#[tokio::test]
async fn given_document_with_page_count_but_no_other_fields_when_execute_then_both_writes_happen()
{
    // format_kind is always Some whenever extraction identifies the file
    // as PDF/EPUB at all, so even "no title/author/year" still triggers
    // the metadata write — this test proves that, distinct from the
    // audio/image sibling tests where an all-empty tag set skips the
    // metadata write entirely.
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.pdf", "a.pdf", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let document_tags = FakeDocumentMetadataReader::new();
    document_tags.seed(
        "/library/a.pdf",
        DocumentTags {
            title: None,
            author: None,
            year: None,
            format_kind: Some(FormatKind::Book),
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
        document_tags,
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.pdf").expect("indexed");
    assert_eq!(repo_handle.document_page_count_for(a.uuid), Some(10));
    let metadata = repo_handle
        .metadata_for(a.uuid)
        .expect("format_kind alone triggers the metadata write");
    assert_eq!(
        metadata,
        SubtypeMetadata::Document {
            title: None,
            author: None,
            year: None,
            format_kind: Some(FormatKind::Book),
        }
    );
}

#[tokio::test]
async fn given_untagged_document_file_when_execute_then_neither_write_happens() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.pdf", "a.pdf", "h-a")
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
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.pdf").expect("indexed");
    assert_eq!(repo_handle.document_page_count_for(a.uuid), None);
    assert!(repo_handle.metadata_for(a.uuid).is_none());
}

#[tokio::test]
async fn given_non_document_file_when_execute_then_document_reader_never_consulted() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/notes.md", "notes.md", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let audio_tags = FakeAudioMetadataReader::new();
    let image_tags = FakeImageMetadataReader::new();
    let document_tags = FakeDocumentMetadataReader::new();
    let document_tags_handle = document_tags.clone();
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        audio_tags,
        image_tags,
        document_tags,
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    assert_eq!(
        document_tags_handle.call_count(),
        0,
        "the document reader must not be consulted at all for a non-document file"
    );
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p alexandria-core --test catalog -- index::`
Expected: all pass, including the 5 new tests. `cargo build --workspace` will still fail at this point (`services.rs` not yet updated) — that's expected, exactly as it was during both prior slices' equivalent task. If you need real GREEN evidence before `services.rs` is fixed, temporarily add a 7th argument to `services.rs`'s `IndexHandler::new(...)` call using `crate::catalog::document_tags::PdfEpubMetadataReader` (from Task 2 — works via its fully-qualified path even though `services.rs` doesn't import it yet), confirm GREEN, then `git checkout -- crates/alexandria-core/src/services.rs` before committing.

- [ ] **Step 5: `fmt` + `clippy`**

Run: `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` (the exact, full-workspace commands). Expected: the *only* errors, if any, come from `services.rs`'s now-outdated `IndexHandler::new(...)` call (Task 7's job) — paste the real output in your report showing this.

- [ ] **Step 6: Commit**

```bash
git add crates/alexandria-core/src/catalog/commands/index.rs crates/alexandria-core/tests/catalog/index.rs
git commit -m "feat: extract document metadata into page count and subtype fields on first index"
```

---

### Task 7: Wire `PdfEpubMetadataReader` into `services.rs`

**Files:**
- Modify: `crates/alexandria-core/src/services.rs`

**Interfaces:**
- Consumes: `PdfEpubMetadataReader` from Task 2, `IndexHandler<A, R, F, C, M, N, O>::new` from Task 6.

Fixes the compile break Task 6 deliberately left, exactly mirroring both prior slices' equivalent task.

- [ ] **Step 1: Add the import**

In `crates/alexandria-core/src/services.rs`, find this block (the file's imports are alphabetically ordered by full path, so `catalog::commands::soft_delete` and `catalog::fs` are adjacent):

```rust
use crate::catalog::commands::soft_delete::SoftDeleteFileHandler;
use crate::catalog::fs::StdFilesystem;
```

Change to insert the new import between them (`document_tags` sorts alphabetically between `commands` and `fs`):

```rust
use crate::catalog::commands::soft_delete::SoftDeleteFileHandler;
use crate::catalog::document_tags::PdfEpubMetadataReader;
use crate::catalog::fs::StdFilesystem;
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
>;
```

- [ ] **Step 3: Update the construction site**

Find:

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

Change to:

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
git commit -m "feat: wire PdfEpubMetadataReader into DefaultIndexHandler"
```

---

### Task 8: HTTP/FFI integration + parity test

**Files:**
- Modify: `crates/alexandria-ffi/Cargo.toml`
- Modify: `crates/alexandria-ffi/tests/parity.rs`

**Interfaces:**
- Consumes: the full extraction pipeline through both surfaces (unchanged public API — this task only adds a new test).

Reuses the exact `local_settings()` / `seed_session()` / `build_services()` / `app()` / `alexandria_index_init` / `alexandria_index_start` / `alexandria_file_get_by_uuid` scaffolding every other test in this file uses. Reuses the same fixture-generation approach as Task 2's unit tests (real PDF built with `lopdf`, real EPUB zip built with the `zip` crate). **Both legs must poll on every column the test asserts on before proceeding to the GET/`alexandria_file_get_by_uuid` call** — the image slice's final review found and fixed a residual race where a test polled on one extraction write but asserted on a different, later one; do not repeat that mistake here. This slice writes up to two independent columns per format (`documents.page_count` for PDF; `documents.title`/`author`/`format_kind` for both) in up to two separate transactions, so the wait condition must require every column the assertions check.

- [ ] **Step 1: Add `lopdf` as an FFI dev-dependency**

The test this task adds only builds a PDF fixture (via `lopdf`) — it deliberately uses PDF rather than EPUB so it exercises both independent extraction writes (page count + metadata) in one test, per the note above. So only `lopdf` is needed here, not `epub` or `zip` (`epub` is read-only and isn't needed for writing a fixture; `zip` is only needed by the EPUB-fixture helper, which this test doesn't use).

In `crates/alexandria-ffi/Cargo.toml`'s `[dev-dependencies]` section (currently `alexandria-core`, `alexandria-http`, `axum`, `chrono`, `image`, `little_exif`, `lofty`, `serde_json`, `sqlx`, `tempfile`, `tokio`, `tower`), add:

```toml
lopdf.workspace = true
```

(alphabetically after `little_exif`, before `lofty`).

- [ ] **Step 2: Write the test**

Append to the end of `crates/alexandria-ffi/tests/parity.rs`. This mirrors the image slice's `given_tagged_image_file_when_indexed_via_http_and_ffi_then_extracted_metadata_matches` test structure closely — read that test first (search for it in this file) to confirm the exact current shape of `local_settings()`, `seed_session()`, `setup_ffi_db()`, and the FFI-leg polling pattern (a `spawn_blocking` closure with its own `tokio::runtime::Runtime` connecting directly to the FFI database file), then write this new test following that same shape with the helpers below substituted in.

```rust
/// Build a minimal valid PDF with `lopdf` — mirrors the identical helper
/// in `alexandria-core`'s `catalog::document_tags` unit tests.
fn write_minimal_pdf(path: &std::path::Path, title: &str, author: &str) {
    use lopdf::{dictionary, Document, Object};

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();

    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
    });
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font_id },
    });
    let content = lopdf::content::Content {
        operations: vec![],
    };
    let content_id = doc.add_object(lopdf::Stream::new(
        dictionary! {},
        content.encode().expect("encode content"),
    ));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "Contents" => content_id,
        "Resources" => resources_id,
        "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1,
        }),
    );
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);

    let info_id = doc.add_object(dictionary! {
        "Title" => Object::string_literal(title),
        "Author" => Object::string_literal(author),
    });
    doc.trailer.set("Info", info_id);

    doc.save(path).expect("save pdf");
}

/// Poll until `documents.title`/`documents.author`/`documents.page_count`
/// are all non-NULL for the named file — proves BOTH extraction writes
/// landed (metadata write and page-count write are separate
/// transactions), not just file-row existence or a single write.
async fn wait_for_http_document_extraction(pool: &sqlx::sqlite::SqlitePool, name: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let row: Option<(Option<String>, Option<String>, Option<i64>)> = sqlx::query_as(
            "SELECT documents.title, documents.author, documents.page_count FROM documents \
             JOIN files ON files.id = documents.file_id \
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
            panic!("http never wrote extracted document metadata");
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Issue #44 document slice parity — index a tagged PDF through both
/// transports and assert the extracted title/author/formatKind/pageCount
/// (written by the indexer itself, not by a manual PATCH) are
/// byte-for-byte identical. PDF is used (not EPUB) because it's the format
/// that exercises both independent writes (page_count + metadata) at once.
#[tokio::test]
async fn given_tagged_pdf_file_when_indexed_via_http_and_ffi_then_extracted_metadata_matches() {
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    let http_doc = http_lib.path().join("book.pdf");
    write_minimal_pdf(&http_doc, "Parity Title", "Parity Author");

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
    wait_for_http_document_extraction(&http_pool, "book.pdf").await;

    let (http_uuid,): (String,) = sqlx::query_as("SELECT uuid FROM files WHERE name = ?")
        .bind("book.pdf")
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
    let ffi_doc = ffi_lib.path().join("book.pdf");
    write_minimal_pdf(&ffi_doc, "Parity Title", "Parity Author");
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

        let rt = tokio::runtime::Runtime::new().unwrap();
        let ffi_uuid: String = rt.block_on(async {
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect(&format!("sqlite://{ffi_db_for_poll}?mode=rw"))
                .await
                .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let row: Option<(String, Option<String>, Option<String>, Option<i64>)> =
                    sqlx::query_as(
                        "SELECT files.uuid, documents.title, documents.author, documents.page_count \
                         FROM documents \
                         JOIN files ON files.id = documents.file_id \
                         WHERE files.name = ?",
                    )
                    .bind("book.pdf")
                    .fetch_optional(&pool)
                    .await
                    .unwrap();
                if let Some((uuid, Some(_), Some(_), Some(_))) = row {
                    return uuid;
                }
                if std::time::Instant::now() > deadline {
                    panic!("ffi never wrote extracted document metadata");
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
    assert_eq!(http_body["pageCount"], ffi_body["pageCount"]);
    assert_eq!(http_body["metadata"], ffi_body["metadata"]);
    assert_eq!(http_body["pageCount"], 1);
    assert_eq!(http_body["metadata"]["title"], "Parity Title");
    assert_eq!(http_body["metadata"]["author"], "Parity Author");
    assert_eq!(http_body["metadata"]["formatKind"], "book");
}
```

Before finalizing: check whether `FileView`'s `page_count` field serializes as `pageCount` or `page_count` in the actual JSON — `FileView` derives plain `Serialize` with no `#[serde(rename_all = "camelCase")]` visible on the struct itself in Task 5's edit (its sibling fields `width`/`height` serialize as-is, lowercase, no case conversion needed since they're already single words). Since `page_count` is a two-word snake_case Rust field name with no `#[serde(rename = ...)]` attribute added in Task 5, it will serialize literally as `"page_count"` in JSON, **not** `"pageCount"` — adjust every `http_body["pageCount"]`/`ffi_body["pageCount"]` reference above to `http_body["page_count"]`/`ffi_body["page_count"]` to match. (This mirrors `width`/`height` staying lowercase with no camelCase conversion — the same will be true for `page_count`, which stays snake_case since nothing renames it.)

- [ ] **Step 3: Run test to verify it passes**

Run: `cargo test -p alexandria-ffi --test parity given_tagged_pdf_file_when_indexed_via_http_and_ffi -- --nocapture`
Expected: `test result: ok. 1 passed`. As with both prior slices' equivalent task, this only exercises code paths built in Tasks 1–7, so there's no meaningful "write it failing first" step — the assertions either hold given the prior tasks' implementation, or reveal a real bug in it.

- [ ] **Step 4: Run the full parity suite to confirm no regression**

Run: `cargo test -p alexandria-ffi --test parity`
Expected: every test in the file passes.

- [ ] **Step 5: `fmt` + `clippy`**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings` (exact literal commands). Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/alexandria-ffi/Cargo.toml crates/alexandria-ffi/tests/parity.rs
git commit -m "test: add HTTP/FFI parity coverage for extracted document metadata"
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
git push -u origin feature/document-metadata-extraction
```

```bash
gh pr create --title "feat: extract document metadata during indexing (issue #44 document slice)" --body "$(cat <<'EOF'
## Summary
- Implements the document slice of issue #44: indexing now reads embedded PDF `/Info` metadata (via `lopdf`) and EPUB OPF metadata (via the `epub` crate), pre-populating title, author, format kind, and (PDF only) page count, instead of leaving every field for the owner to enter manually.
- Format scope: PDF and EPUB only — `.mobi`/`.azw`/`.azw3` have no workable pure-Rust library and stay unextracted, same graceful degradation as audio's `.wma` and image's non-EXIF formats.
- `format_kind` is set from which parser matched (Book/Ebook), independent of whether title/author were found. `page_count` only ever comes from PDF (EPUB's reflowable text has no real page count) and needed the same narrow new repository method + `FileView` field pattern the image slice established for `width`/`height`.
- Extraction runs once, at first index only; `refresh.rs` is untouched. Extraction failure never fails the indexing run.
- Video and comic extraction are separate follow-up slices, in that order.

See \`docs/superpowers/specs/2026-08-06-document-metadata-extraction-design.md\` for the full design.

Relates to #44 (does not close it — this is the document slice only).

## Test plan
- [x] \`cargo test --workspace\` — all green
- [x] \`cargo fmt --all\` / \`cargo clippy --workspace --all-targets -- -D warnings\`
- [x] Unit tests: \`PdfEpubMetadataReader\` against real generated PDF (via \`lopdf\`) and EPUB (via hand-built zip) fixtures, repository \`set_document_page_count\`/\`find_document_page_count\`, \`BrowseFilesHandler::get_by_uuid\` page_count wiring, \`IndexHandler\` against \`FakeDocumentMetadataReader\` (PDF full/EPUB full/format_kind-only/untagged/non-document, with a call-count assertion proving the reader is never consulted for non-document files)
- [x] HTTP/FFI parity test: index a real tagged PDF through both surfaces, assert extracted page count + metadata match (race-free — both legs poll on all extraction writes landing, not just file-row existence)

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
