# Design: Extract image metadata during indexing (2nd slice of issue #44)

**Date:** 2026-08-06
**Status:** Approved, ready for implementation planning
**Tracks:** [Issue #44](https://github.com/artur-rios/alexandria-api/issues/44) — image scope only

## Context

Issue #44 tracks reading embedded type-specific metadata at index time across
five file-type families. The audio slice shipped in
[PR #80](https://github.com/artur-rios/alexandria-api/pull/80), establishing
the pattern: a read-only `<Type>MetadataReader` trait port wired as a generic
`IndexHandler` collaborator, extraction running once at first index only, and
extraction failure never failing the indexing run.

This design covers the second slice: **image**, via EXIF. The remaining two
(document, video) are separate follow-up design/plan/implementation cycles,
in that order, per the decomposition agreed for the rest of issue #44.

## Decisions

1. **Reader library: `kamadak-exif`** (crate name `exif`), a pure-Rust,
   actively-maintained EXIF reader. Parses JPEG, TIFF, HEIC, and PNG's `eXIf`
   chunk (recent PNG spec). Of the 9 extensions `classify_by_extension` maps
   to `FileType::Image` (jpg/jpeg/png/gif/webp/bmp/tif/tiff/svg), this covers
   jpg/jpeg/tif/tiff and PNG-with-EXIF; gif/webp/bmp/svg have no EXIF to
   extract and always yield no metadata — the same graceful degradation the
   audio slice already established for `.wma`, just covering a larger
   fraction of files this time.
2. **`width`/`height` need a new, narrow repository method.** Unlike audio
   (where every extractable field was already editable via UC-04, hence
   writable through the existing `update_metadata`), EXIF's most reliably
   available data — pixel dimensions — lives in the `images` table's
   `width`/`height` columns, which `SubtypeMetadata::Image` does not cover
   (only `title`/`caption` are owner-editable, per `catalog/model.rs`'s
   `SubtypeMetadata` doc comment: `width`/`height` are explicitly
   "non-editable, never touched" by `update_metadata`). This design adds one
   deliberate, minimal deviation from the "zero repository changes" pattern:
   `CatalogRepository::set_image_dimensions(uuid, width, height)`.
3. **`caption` is dropped from what extraction populates.** EXIF has no
   dedicated caption tag (that's an IPTC/XMP concept — out of scope per the
   issue, which scopes this to EXIF only). `title` maps from EXIF's
   `ImageDescription` tag when present (uncommon in practice — most cameras
   don't set it — but real when a camera or editor did set it). `caption`
   remains owner-only, exactly as before this feature.
4. **Extraction still runs once, at first index only**, same as audio;
   `refresh.rs` stays untouched.
5. **Extraction failure is still never a run failure.** No EXIF, a corrupt
   file, or an unsupported format (gif/webp/bmp/svg) all collapse to `None`
   from the reader — never `Err`, never counted in `IndexOutcome::failed`.

## Architecture

### New port: `ImageMetadataReader`

`crates/alexandria-core/src/catalog/image_tags.rs` (new file), mirroring
`audio_tags.rs`'s shape:

```rust
pub struct ImageTags {
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub title: Option<String>,
}

#[allow(async_fn_in_trait)]
pub trait ImageMetadataReader: Send + Sync {
    /// Best-effort read of embedded EXIF data. `None` covers "no EXIF
    /// present" and "couldn't parse this file" alike — the caller never
    /// needs to tell them apart.
    async fn read(&self, path: &str) -> Option<ImageTags>;
}
```

Concrete implementation `ExifImageMetadataReader` wraps `kamadak-exif`'s
`exif::Reader`, reading `ImageWidth`/`PixelXDimension`,
`ImageLength`/`PixelYDimension` (falling back to the compressed-image
`PixelXDimension`/`PixelYDimension` tags when the uncompressed
`ImageWidth`/`ImageLength` tags are absent, which is common for JPEG), and
`ImageDescription`. Any parse error or missing EXIF block is logged at
`debug` and mapped to `None`.

### New repository method

`CatalogRepository` gains:

```rust
/// Write an image file's pixel dimensions (issue #44 image pilot). Unlike
/// `update_metadata`, this touches `images.width`/`images.height` directly
/// — columns `SubtypeMetadata::Image` deliberately excludes because they
/// are not owner-editable (UC-04). Returns `NotFound` when no file row
/// carries the UUID.
async fn set_image_dimensions(&self, uuid: Uuid, width: i64, height: i64) -> Result<(), DomainError>;
```

Sqlite implementation mirrors `update_metadata`'s existing shape (resolve `id`/`type` from `uuid` in a transaction, verify `FileType::Image`, `UPDATE`, check `rows_affected`).

### New repository read method and `FileView` field addition

Discovered while planning: `width`/`height` currently have **no read path at
all** — no query selects them, and `FileView` (UC-03's single-file response,
`{ file, metadata }`) has nowhere to carry them, since `SubtypeMetadata::Image`
deliberately excludes them. Writing them via extraction with no way to read
them back would make this data invisible to every caller. Fix, chosen to
keep the blast radius minimal (a sibling read method rather than changing
`find_metadata_by_uuid`'s existing return type, which many call sites
depend on):

```rust
/// Read an image file's pixel dimensions, if both are set (issue #44 image
/// pilot). `None` when the file doesn't exist, isn't an image, or either
/// column is still NULL (extraction never ran, or found no dimensions).
async fn find_image_dimensions(&self, uuid: Uuid) -> Result<Option<(i64, i64)>, DomainError>;
```

`FileView` (`catalog/model.rs`) gains two fields:

```rust
pub struct FileView {
    pub file: File,
    pub metadata: Option<SubtypeMetadata>,
    pub width: Option<i64>,
    pub height: Option<i64>,
}
```

`width`/`height` are `None` for every non-image file and for an image file
whose dimensions haven't been extracted yet. `BrowseFilesHandler::get_by_uuid`
(`catalog/queries/browse.rs`) calls `find_image_dimensions` alongside its
existing `find_metadata_by_uuid` call, only when `file.file_type ==
FileType::Image`, and threads the result into `FileView`.

### `IndexHandler` wiring

`IndexHandler<A, R, F, C, M, N>` gains a 6th generic parameter, `N: ImageMetadataReader`, alongside audio's existing `M: AudioMetadataReader`. `index_entry` adds a parallel `FileType::Image` branch with two independent, best-effort writes:

```rust
if file_type == FileType::Image {
    if let Some(tags) = self.image_tags.read(&entry.path).await {
        if let (Some(width), Some(height)) = (tags.width, tags.height) {
            if let Err(err) = self.repo.set_image_dimensions(file.uuid, width, height).await {
                tracing::warn!(path = %entry.path, error = %err, "indexed but failed to write extracted image dimensions");
            }
        }
        if let Some(title) = tags.title {
            let metadata = SubtypeMetadata::Image { title: Some(title), caption: None };
            if let Err(err) = self.repo.update_metadata(file.uuid, &metadata).await {
                tracing::warn!(path = %entry.path, error = %err, "indexed but failed to write extracted image title");
            }
        }
    }
}
```

Both writes are independent — a dimensions-write failure doesn't block the title write or vice versa — and neither one fails indexing or increments `IndexOutcome::failed` (decision 5). `services.rs` wires the real `ExifImageMetadataReader` alongside the existing `LoftyAudioMetadataReader`.

## Error handling / failure isolation

- `ImageMetadataReader::read` never returns `Err`; every failure mode (no EXIF block, corrupt data, unsupported container) collapses to `None`.
- Both repository write failures (`set_image_dimensions`, `update_metadata`) are logged at `warn` and swallowed independently — neither propagates, neither is counted as an indexing failure.
- `kamadak-exif` is designed for untrusted input and returns `Result` rather than panicking, so no extra guarding beyond the `Option`-collapsing described.

## Testing strategy

1. **Unit tests** (`IndexHandler` against fakes): `FakeImageMetadataReader` (mirrors `FakeAudioMetadataReader`, including a `call_count()` for the "reader never consulted for non-image files" test) and `FakeCatalogRepository` extended with `set_image_dimensions` + a `dimensions_for(uuid)` inspector. Cases: dimensions + title both present → both writes happen; dimensions only (the common case — most files have no `ImageDescription`) → only `set_image_dimensions` called; no EXIF at all → neither write happens; non-image file → reader never consulted.
2. **`ExifImageMetadataReader` unit test** — against a real fixture. Unlike `lofty` (which could both read and write, letting the audio slice generate its test WAV in code), `kamadak-exif` is read-only, and no EXIF-*writing* crate is otherwise needed by this workspace. This design adds one small, genuinely minimal checked-in binary fixture — a real JPEG a few hundred bytes to a few KB in size with a hand-verified EXIF block (dimensions + an `ImageDescription`) — at `crates/alexandria-core/tests/fixtures/tagged.jpg`, generated once during implementation.
3. **HTTP/FFI integration + parity** — index that same fixture through both surfaces, assert `GET /v1/files/{uuid}` (and its FFI equivalent) return the extracted `width`/`height`/`title`, using the same race-free polling pattern (poll on the actual write landing, not just file-row existence) the audio slice's final review established.

## Out of scope (this slice)

- Document (PDF/EPUB), Video (resolution/`mediaKind`), Comic
  (`ComicInfo.xml`) — separate design/plan/implementation cycles, in that
  order.
- `caption` extraction — no EXIF-native field maps to it; stays owner-only.
- IPTC/XMP metadata (which does have caption-like fields) — explicitly out
  of scope per issue #44, which scopes this slice to EXIF only.
- Any provenance/re-extraction behavior on refresh — ruled out by decision 4,
  consistent with the audio slice.
