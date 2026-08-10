# Design: Extract comic metadata during indexing (5th and final slice of issue #44)

**Date:** 2026-08-10
**Status:** Approved, ready for implementation planning
**Tracks:** [Issue #44](https://github.com/artur-rios/alexandria-api/issues/44) — comic scope only

## Context

Issue #44 tracks reading embedded type-specific metadata at index time across
five file-type families. Audio shipped in
[PR #80](https://github.com/artur-rios/alexandria-api/pull/80); image shipped
in [PR #84](https://github.com/artur-rios/alexandria-api/pull/84); document
shipped in [PR #87](https://github.com/artur-rios/alexandria-api/pull/87);
video shipped in [PR #88](https://github.com/artur-rios/alexandria-api/pull/88).
All four established the same pattern: a read-only `<Type>MetadataReader`
trait port wired as a generic `IndexHandler` collaborator, extraction running
once at first index only, extraction failure never failing the run, and —
when a type has data outside the owner-editable `SubtypeMetadata` (image's
`width`/`height`, document's `page_count`, video's `duration_seconds`) — a
narrow new repository write/read method pair plus a `FileView` field
addition to make that data visible to callers.

This design covers the fifth and final slice: **comic**, via `ComicInfo.xml`
and archive-entry counting. With this slice, issue #44 is complete.

## Decisions

1. **Format scope: CBZ only.** `classify_by_extension` maps cbr/cbz to
   `FileType::Comic`. CBZ is a ZIP archive, read via the `zip` crate (already
   a workspace dependency, used for EPUB test fixtures in the document
   slice). CBR is RAR, a proprietary format with no viable pure-Rust reader
   — the only options (FFI bindings to `unrar`, or shelling out to an
   external `unrar` binary) carry licensing restrictions and build
   complexity beyond what this slice's payoff justifies. `.cbr` files index
   normally with no extracted metadata, the same graceful degradation
   document's `.mobi`/`.azw`/`.azw3` established.
2. **Metadata source: `ComicInfo.xml` (when present) plus always-computed
   page count.** `ComicInfo.xml` is the de-facto ComicRack/ComicVine
   metadata standard, commonly bundled inside well-organized CBZ archives
   at the archive root. When present (matched case-insensitively, since
   real-world archives vary), it supplies `<Title>`/`<Series>`/`<Number>`.
   `page_count` is independent of whether the XML exists at all — it's
   always computed by counting image-extension entries
   (`.jpg`/`.jpeg`/`.png`/`.gif`/`.webp`/`.bmp`) in the archive, the same
   list `classify_by_extension` already recognizes as image formats.
3. **`issue_number` parsing is best-effort.** `ComicInfo.xml`'s `<Number>`
   element is a string that isn't always a clean integer (e.g. `"1.5"` for
   specials, `"Annual"`). A failed integer parse leaves `issue_number`
   `None` rather than erroring — consistent with every other slice's
   "collapse to `None`, never `Err`" rule.
4. **`title`/`series`/`issue_number` reuse the existing owner-editable
   columns; only `page_count` needs new plumbing.** `comic_books` already
   has `title`, `series`, `issue_number` columns, all owner-editable via
   UC-04's `SubtypeMetadata::Comic`. Extraction writes these through the
   existing `update_metadata`. `page_count` has no owner-editable path —
   it needs the same "outside `SubtypeMetadata`" pattern image's
   `width`/`height`, document's `page_count`, and video's
   `duration_seconds` established. Like document's `page_count` (and
   unlike video's `duration_seconds`), `comic_books.page_count` already
   exists in the original schema migration — no new migration needed.
5. **`FileView`'s new field is named `comic_page_count`, not `page_count`.**
   `FileView` is one flat struct shared across every file type, and
   document's slice already claimed `page_count` for its own extracted
   field. The two are never both non-`None` for the same file (one gates
   on `FileType::Document`, the other on `FileType::Comic`), but reusing
   one field name for two semantically distinct values on the same struct
   would leave API consumers unable to tell, from the field name alone,
   which subtype a given `page_count` describes. `comic_page_count` avoids
   the ambiguity.
6. **Extraction still runs once, at first index only**; `refresh.rs` stays
   untouched.
7. **Extraction failure is still never a run failure.** A corrupt zip, a
   missing or malformed `ComicInfo.xml`, or an unsupported extension
   (`.cbr`) all collapse to `None`/best-effort partial results — never
   `Err`, never counted in `IndexOutcome::failed`.

## Architecture

### New port: `ComicMetadataReader`

`crates/alexandria-core/src/catalog/comic_tags.rs` (new file), mirroring
the prior four slices' shape:

```rust
pub struct ComicTags {
    pub title: Option<String>,
    pub series: Option<String>,
    pub issue_number: Option<i64>,
    pub page_count: Option<i64>,
}

#[allow(async_fn_in_trait)]
pub trait ComicMetadataReader: Send + Sync {
    /// Best-effort read of embedded comic metadata. `None` covers
    /// "couldn't open the archive" only — a readable archive with no
    /// `ComicInfo.xml` still yields `Some` with `page_count` set and the
    /// other three fields `None`.
    async fn read(&self, path: &str) -> Option<ComicTags>;
}
```

Concrete implementation `CbzComicMetadataReader` opens the file with the
`zip` crate, scans entries once to (a) find a `ComicInfo.xml`/`comicinfo.xml`
entry if present and (b) count image-extension entries for `page_count`.
When found, `ComicInfo.xml` is parsed with `quick-xml` (new dependency — no
`serde` derive needed given the tiny, flat `<ComicInfo><Title>…</Title>
<Series>…</Series><Number>…</Number></ComicInfo>` shape) for `<Title>`,
`<Series>`, `<Number>` (parsed to `i64`, `None` on failure). Any failure to
open the zip at all yields `None`; a missing or unparseable `ComicInfo.xml`
still yields `Some(ComicTags { page_count: Some(_), .. })` since page
counting doesn't depend on it.

### New repository method and `FileView` field addition

`CatalogRepository` gains:

```rust
/// Write a comic file's page count (issue #44 comic slice). Unlike
/// `update_metadata`, this touches `comic_books.page_count` directly —
/// `SubtypeMetadata::Comic` deliberately excludes it because it is not
/// owner-editable (UC-04). Returns `NotFound` when no file row carries the
/// UUID, `InvalidInput` when the file is not a comic.
async fn set_comic_page_count(&self, uuid: Uuid, page_count: i64) -> Result<(), DomainError>;

/// Read a comic file's page count, if set (issue #44 comic slice). `None`
/// when the file doesn't exist, isn't a comic, or the column is still
/// `NULL` (extraction never ran, or the archive couldn't be opened).
async fn find_comic_page_count(&self, uuid: Uuid) -> Result<Option<i64>, DomainError>;
```

`FileView` (`catalog/model.rs`) gains a fifth extraction-related field,
`comic_page_count: Option<i64>` (see decision 5 for the naming rationale).
`BrowseFilesHandler::get_by_uuid` calls `find_comic_page_count` alongside
its existing calls, only when `file.file_type == FileType::Comic`.

### `IndexHandler` wiring

`IndexHandler<A, R, F, C, M, N, O, P, Q>` gains a 9th generic parameter,
`Q: ComicMetadataReader`, alongside audio's `M`, image's `N`, document's
`O`, and video's `P`. `index_entry` gets a parallel `FileType::Comic`
branch with two independent, best-effort writes:

```rust
if file_type == FileType::Comic {
    if let Some(tags) = self.comic_tags.read(&entry.path).await {
        if let Some(page_count) = tags.page_count {
            // set_comic_page_count, warn+swallow on failure — never
            // counted in IndexOutcome::failed.
        }
        if tags.title.is_some() || tags.series.is_some() || tags.issue_number.is_some() {
            // update_metadata with SubtypeMetadata::Comic{ title, series,
            // issue_number }, warn+swallow on failure.
        }
    }
}
```

Both writes are independent; neither failing blocks the other or fails
indexing. `services.rs` wires the real `CbzComicMetadataReader` alongside
the existing audio/image/document/video readers.

## Error handling / failure isolation

- `ComicMetadataReader::read` never returns `Err`; the only failure mode
  that collapses the whole result to `None` is an unopenable archive.
  Missing/malformed `ComicInfo.xml` degrades gracefully to a partial
  `Some` result (page count still present).
- Both repository write failures (`set_comic_page_count`,
  `update_metadata`) are logged at `warn` and swallowed independently —
  neither propagates, neither is counted as an indexing failure.
- The `zip` crate and `quick-xml` are both designed for untrusted input
  and return `Result` rather than panicking, so no extra guarding beyond
  the `Option`-collapsing described.

## Testing strategy

1. **Unit tests** (`IndexHandler` against fakes): `FakeComicMetadataReader`
   (mirrors the prior four fakes, including a `call_count()` for the
   "reader never consulted for non-comic files" test) and
   `FakeCatalogRepository` extended with `set_comic_page_count` + a
   `comic_page_count_for(uuid)` inspector. Cases: full tags (title +
   series + issue_number + page_count) → both writes happen;
   issue_number-only (no title/series, but issue_number alone triggers
   the metadata write, page_count still written independently) → both
   writes happen; page_count-only (no `ComicInfo.xml` at all) → only the
   page-count write happens; unopenable archive → neither write; non-comic
   file → reader never consulted.
2. **`CbzComicMetadataReader` unit test** — against real generated
   fixtures. The `zip` crate can both read and write, mirroring `lopdf`'s
   dual role in the document slice: one fixture with a hand-written
   `ComicInfo.xml` entry plus a couple of dummy `.jpg` entries (title +
   series + issue_number + page_count = 2 all extracted), one fixture with
   only the `.jpg` entries and no `ComicInfo.xml` (page_count extracted,
   the other three `None`) — both built at test time, no checked-in
   binaries.
3. **HTTP/FFI integration + parity** — index a tagged CBZ fixture through
   both surfaces, assert `GET /v1/files/{uuid}` (and its FFI equivalent)
   return the extracted `title`/`series`/`issueNumber`/`comicPageCount` —
   using the same race-free polling pattern (poll on every column the test
   asserts on, not just the first one to land) established by the image
   slice's final review and carried through every slice since.

## Out of scope (this slice)

- `.cbr` (RAR) extraction — no viable pure-Rust library; these files
  continue to index with no metadata, same as before this slice.
- Any provenance/re-extraction behavior on refresh — ruled out by decision
  6, consistent with every prior slice.
- Cover-image extraction, page-by-page metadata, or any per-page data
  beyond a total count — none of this is part of `SubtypeMetadata::Comic`
  or `FileView` today, and adding it would be a separate, larger design
  decision.

## Issue #44 completion

This is the fifth and final slice of issue #44. Once merged, every file
type (`audio`, `video`, `document`, `image`, `comic`) has best-effort
metadata extraction at first index, and issue #44 can be closed.
