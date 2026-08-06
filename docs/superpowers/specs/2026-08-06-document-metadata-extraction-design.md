# Design: Extract document metadata during indexing (3rd slice of issue #44)

**Date:** 2026-08-06
**Status:** Approved, ready for implementation planning
**Tracks:** [Issue #44](https://github.com/artur-rios/alexandria-api/issues/44) — document scope only

## Context

Issue #44 tracks reading embedded type-specific metadata at index time across
five file-type families. Audio shipped in
[PR #80](https://github.com/artur-rios/alexandria-api/pull/80); image shipped
in [PR #84](https://github.com/artur-rios/alexandria-api/pull/84),
establishing the pattern: a read-only `<Type>MetadataReader` trait port wired
as a generic `IndexHandler` collaborator, extraction running once at first
index only, extraction failure never failing the run, and — when a type has
data outside the owner-editable `SubtypeMetadata` (image's `width`/`height`)
— a narrow new repository write/read method pair plus a `FileView` field
addition to make that data visible to callers.

This design covers the third slice: **document**, via PDF and EPUB parsing.
Video and comic remain separate follow-up design/plan/implementation cycles.

## Decisions

1. **Format scope: PDF and EPUB only.** `classify_by_extension` maps
   pdf/epub/mobi/azw/azw3 to `FileType::Document`. `.mobi`/`.azw`/`.azw3`
   (proprietary Amazon Kindle formats) have no workable pure-Rust parsing
   library and are out of scope — they still index normally, they simply
   never get extracted metadata, the same graceful degradation already
   established for audio's `.wma` and image's non-EXIF formats (gif/webp/
   bmp/svg).
2. **Libraries: `lopdf` for PDF, `epub` for EPUB.** Both are pure Rust.
   `lopdf` reads the PDF `/Info` dictionary (title, author) and the page
   tree (page count) directly. `epub` reads OPF metadata (title, creator).
   One concrete reader, `PdfEpubMetadataReader`, dispatches internally by
   file extension — unlike audio/image (one library covers every extension
   of that type), document needs two different parsers behind one port.
3. **`format_kind` is set from which parser matched**, not from embedded
   metadata: PDF → `FormatKind::Book`, EPUB → `FormatKind::Ebook`. This is a
   reliable, free signal — unlike `title`/`author`/`year`, it never depends
   on what the file actually embeds, so it's set whenever extraction
   identifies the file as PDF or EPUB at all.
4. **`page_count` only ever comes from PDF.** EPUB is reflowable text with
   no fixed pages; approximating a count from its spine/section list would
   be a misleading number presented as a real one. EPUB extraction always
   leaves `page_count` `None`.
5. **`page_count` needs a new, narrow repository method**, mirroring
   image's `width`/`height` precedent: it lives in the `documents` table's
   `page_count` column, which `SubtypeMetadata::Document` deliberately
   excludes (only `title`/`author`/`year`/`format_kind` are owner-editable
   via UC-04). Adds `CatalogRepository::set_document_page_count` (write) and
   `find_document_page_count` (read), plus a `page_count` field on
   `FileView`, gated on `FileType::Document` exactly as `width`/`height` are
   gated on `FileType::Image`.
6. **Extraction still runs once, at first index only**; `refresh.rs` stays
   untouched.
7. **Extraction failure is still never a run failure.** A corrupt PDF, a
   malformed EPUB zip, a missing `/Info` dict or OPF metadata block, or an
   unsupported extension (mobi/azw/azw3) all collapse to `None` — never
   `Err`, never counted in `IndexOutcome::failed`.

## Architecture

### New port: `DocumentMetadataReader`

`crates/alexandria-core/src/catalog/document_tags.rs` (new file), mirroring
`audio_tags.rs`/`image_tags.rs`'s shape:

```rust
pub struct DocumentTags {
    pub title: Option<String>,
    pub author: Option<String>,
    pub year: Option<i64>,
    pub format_kind: Option<FormatKind>,
    pub page_count: Option<i64>,
}

#[allow(async_fn_in_trait)]
pub trait DocumentMetadataReader: Send + Sync {
    /// Best-effort read of embedded document metadata. `None` covers "not a
    /// supported format (mobi/azw/azw3)", "no metadata present", and
    /// "couldn't parse this file" alike — the caller never needs to tell
    /// them apart.
    async fn read(&self, path: &str) -> Option<DocumentTags>;
}
```

Concrete implementation `PdfEpubMetadataReader` branches on the file's
extension: `.pdf` → `lopdf`, reading the trailer's `/Info` dictionary for
`Title`/`Author` and walking the page tree's `/Count` for page count;
`.epub` → the `epub` crate, reading OPF `<dc:title>`/`<dc:creator>` and any
publication-year metadata; anything else (`.mobi`/`.azw`/`.azw3`, or a
`.pdf`/`.epub` that fails to parse) → `None`. `format_kind` is set
unconditionally to `Book`/`Ebook` the moment either branch is entered,
independent of whether title/author/year were found.

### New repository method and `FileView` field addition

`CatalogRepository` gains:

```rust
/// Write a document file's page count (issue #44 document slice). Unlike
/// `update_metadata`, this touches `documents.page_count` directly —
/// `SubtypeMetadata::Document` deliberately excludes it because it is not
/// owner-editable (UC-04). Returns `NotFound` when no file row carries the
/// UUID, `InvalidInput` when the file is not a document.
async fn set_document_page_count(&self, uuid: Uuid, page_count: i64) -> Result<(), DomainError>;

/// Read a document file's page count, if set (issue #44 document slice).
/// `None` when the file doesn't exist, isn't a document, or the column is
/// still `NULL` (extraction never ran, found no PDF page tree, or the file
/// was EPUB — EPUB never sets this).
async fn find_document_page_count(&self, uuid: Uuid) -> Result<Option<i64>, DomainError>;
```

`FileView` (`catalog/model.rs`) gains a third field, `page_count: Option<i64>`,
`None` for every non-document file. `BrowseFilesHandler::get_by_uuid`
(`catalog/queries/browse.rs`) calls `find_document_page_count` alongside its
existing `find_metadata_by_uuid`/`find_image_dimensions` calls, only when
`file.file_type == FileType::Document`.

### `IndexHandler` wiring

`IndexHandler<A, R, F, C, M, N, O>` gains a 7th generic parameter,
`O: DocumentMetadataReader`, alongside audio's `M` and image's `N`.
`index_entry` gets a parallel `FileType::Document` branch with two
independent, best-effort writes:

```rust
if file_type == FileType::Document {
    if let Some(tags) = self.document_tags.read(&entry.path).await {
        if let Some(page_count) = tags.page_count {
            // set_document_page_count, warn+swallow on failure — never
            // counted in IndexOutcome::failed.
        }
        if tags.title.is_some()
            || tags.author.is_some()
            || tags.year.is_some()
            || tags.format_kind.is_some()
        {
            // update_metadata with SubtypeMetadata::Document{ title, author,
            // year, format_kind }, warn+swallow on failure.
        }
    }
}
```

Since `format_kind` is set whenever the reader identifies the file as PDF
or EPUB at all, the metadata write fires on essentially every successful
extraction — even a document with no embedded title/author/year still gets
`format_kind` pre-filled. Both writes are independent; neither failing
blocks the other or fails indexing. `services.rs` wires the real
`PdfEpubMetadataReader` alongside the existing audio/image readers.

## Error handling / failure isolation

- `DocumentMetadataReader::read` never returns `Err`; every failure mode
  (unsupported extension, corrupt PDF, malformed EPUB zip, missing
  `/Info`/OPF metadata) collapses to `None`.
- Both repository write failures (`set_document_page_count`,
  `update_metadata`) are logged at `warn` and swallowed independently —
  neither propagates, neither is counted as an indexing failure.
- Both `lopdf` and `epub` are designed for untrusted input and return
  `Result` rather than panicking, so no extra guarding beyond the
  `Option`-collapsing described.

## Testing strategy

1. **Unit tests** (`IndexHandler` against fakes): `FakeDocumentMetadataReader`
   (mirrors `FakeAudioMetadataReader`/`FakeImageMetadataReader`, including a
   `call_count()` for the "reader never consulted for non-document files"
   test) and `FakeCatalogRepository` extended with `set_document_page_count`
   + a `document_page_count_for(uuid)` inspector. Cases: full tags (title +
   author + year + format_kind + page_count) → both writes happen; PDF with
   only page_count found (no title/author/year, but `format_kind` still
   `Some`) → both writes still happen (format_kind alone is enough to
   trigger the metadata write); EPUB (page_count always `None`) → only the
   metadata write happens; no metadata at all → neither write; non-document
   file → reader never consulted.
2. **`PdfEpubMetadataReader` unit test** — against real generated fixtures.
   Unlike audio (`lofty` could both read and write its own test WAV), this
   reader is read-only per format, but both `lopdf` and the EPUB toolchain
   can *write* their own minimal test files: a tiny PDF built directly with
   `lopdf`'s document-building API (a trailer, one page, an `/Info` dict)
   and a tiny EPUB built by zipping a hand-constructed `mimetype` +
   `META-INF/container.xml` + a minimal OPF file with `<dc:title>`/
   `<dc:creator>` — both generated in the test, no checked-in binaries.
3. **HTTP/FFI integration + parity** — index both fixtures (one PDF, one
   EPUB) through both surfaces, assert `GET /v1/files/{uuid}` (and its FFI
   equivalent) return the extracted `title`/`author`/`year`/`formatKind`/
   `pageCount` — using the same race-free polling pattern (poll on the
   actual write landing across *every* column the test asserts on, not just
   the first one to land) that image's final review established after
   finding a residual race there.

## Out of scope (this slice)

- Video (resolution/`mediaKind`), Comic (`ComicInfo.xml`) — separate
  design/plan/implementation cycles, in that order.
- `.mobi`/`.azw`/`.azw3` extraction — no workable pure-Rust library; these
  extensions continue to index with no metadata, same as before this slice.
- EPUB page/section counting as a `page_count` approximation — explicitly
  rejected per decision 4; a misleading number is worse than none.
- Any provenance/re-extraction behavior on refresh — ruled out by decision
  6, consistent with audio and image.
