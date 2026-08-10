# Design: Media playback (F-10 — UC-38, UC-39, UC-40)

**Date:** 2026-08-10
**Status:** Approved, ready for implementation planning
**Tracks:** New capability area — Media Playback (MP). Milestone F-10.

## Context

Alexandria today is catalog-only. It indexes files, extracts and edits their
metadata, groups them, and tracks coarse watch/read progress — but it never
serves a file's *bytes*. The only content-bearing operations are UC-32/UC-33,
which read and write TextFile content as a JSON string.

The Flutter front-end needs to play, view, and read the library. Nothing in the
backend can render anything: playback happens in Flutter's `video_player`,
`just_audio`, image widgets, PDF viewers, and webviews. What this design
specifies is therefore not a player but **what the core, HTTP, and FFI surfaces
hand to a player**.

The Vision Document's out-of-scope list rules out media *editing* (re-encoding,
image manipulation) and byte duplication. Serving unmodified bytes is neither,
so playback fits the product's stated scope — but §1.2 currently says nothing
about playback either way, and that silence is worth closing.

## Decisions

1. **Byte serving plus two type-aware helpers.** A single stream endpoint
   covers all seven file types: Flutter has a mature renderer for each of
   them. Two helpers are added where the server can do something the client
   genuinely can't do as well — extracting one page out of a comic archive,
   and producing a thumbnail. PDF page *rasterization* is deliberately
   excluded: `lopdf` parses structure but cannot render, so rasterizing
   means `pdfium-render`, which bundles a C++ binary and would become a
   **second** system dependency in a project whose Technology Stack Document
   pointedly calls ffmpeg "the one system dependency in the graph". Flutter
   renders PDFs locally, and the FFI client has the file path anyway.

2. **HTTP streams bytes; FFI returns a descriptor.** The FFI surface is
   `c_char*` JSON in, `c_char*` JSON out, `c_int` status, and
   `alexandria-ffi/src/lib.rs` opens with `#![deny(unsafe_code)]`. That shape
   is right for metadata and wrong for a 4 GB movie. FFI callers are Flutter
   *desktop*, on the same machine as the file, so UC-38 over FFI returns the
   resolved absolute path, MIME type, and size; Flutter opens the file
   directly, zero-copy. The rejected alternatives were a raw-pointer
   `read_chunk` FFI (pays a real safety cost to make a parity suite green on
   bytes that are identical on disk either way) and having FFI hand back a
   localhost HTTP URL (collapses the two surfaces' independence — the FFI
   build would require the HTTP server to be running).

3. **Bounded derived artifacts still cross FFI as bytes, base64-encoded.** A
   comic page has no path — it lives inside an archive — and a thumbnail does
   not exist on disk at all. So the line is drawn on *boundedness*, not on
   file-ness: unbounded original bytes travel as a path (UC-38), bounded
   derived artifacts travel as base64 inside the JSON payload (UC-39, UC-40).
   Both stay within the existing FFI shape, and HTTP/FFI parity for UC-39 and
   UC-40 is byte-exact. The three new functions carry the same
   `#[allow(unsafe_code)]` on `#[no_mangle]` that every existing FFI
   function already carries — that targeted allow is the crate's
   established pattern, and no raw pointer buffers or manual lifetimes are
   introduced beyond it.

4. **Comic pages are served without decoding.** A CBZ entry is already a JPEG
   or PNG. UC-39 returns the entry's raw bytes with a MIME type derived from
   the entry's extension. No decode, no re-encode — which is what FR-MP-03
   requires, and it confines the `image` crate to UC-40.

5. **CBZ only, CBR unsupported.** UC-39 follows the precedent
   `comic_tags.rs` already set: CBR is RAR, with no viable pure-Rust reader,
   and a CBR page request is rejected rather than silently degraded.

6. **Thumbnails are cached on disk, keyed by content hash.** Regenerating a
   video keyframe per request makes a 200-item grid unusable. The cache key
   is `contentHash` + max dimension, so re-index invalidation is automatic
   and free: different bytes, different key. This is derived data, not the
   byte duplication the Vision Document excludes. There is no eviction
   policy, deliberately — inventing one before anyone has a full cache would
   be guessing.

7. **No resume position.** Adding `positionSeconds` / `positionPage` to
   WatchProgress and ReadingProgress touches two existing use cases'
   semantics, the data model, migrations, and both surfaces. It is a
   peer-sized piece of work, not a rider on this one, and playback is useful
   without it. It gets its own spec.

## Requirements

New requirement group **MP (Media Playback)** in the System Requirements
Document, §3.8:

| ID | Requirement |
| --- | --- |
| FR-MP-01 | The system shall stream the bytes of an `active` File from its recorded path, with a MIME type derived from the file's extension. |
| FR-MP-02 | The system shall support HTTP byte-range requests over that stream, so a client can seek without transferring the whole file. |
| FR-MP-03 | The system shall never re-encode, transcode, or otherwise modify the bytes it serves. |
| FR-MP-04 | The system shall return a single page of a CBZ ComicBook as an image, addressed by 1-based page index. |
| FR-MP-05 | The system shall return a downscaled thumbnail image for a video, image, or comic File. |
| FR-MP-06 | The system shall expose playback operations via both the HTTP and FFI surfaces. Because the FFI surface cannot carry a byte stream, FR-MP-01 over FFI returns a **playback descriptor** — resolved absolute path, MIME type, and byte size — and parity for it is defined on that descriptor and on the authorization, state, and error decisions rather than on byte transfer. FR-MP-04 and FR-MP-05 return their bytes over both surfaces and are byte-exact across them. |

### Amendments to existing documents

- **FR-FC-24** asserts unqualified identical results across both surfaces.
  It gains a sentence naming FR-MP-06 as the single carve-out, so the two
  requirements do not sit in silent conflict.
- **Vision Document §1.2** gains one clarifying sentence: playback serves
  bytes unmodified and is not media editing, which the out-of-scope list
  already excludes.
- **Use Case Overview diagram** (Use Case Specification Document §1.3) gains
  a "Playback" subgraph with UC-38…UC-40, with UC-38 and UC-39 edged to the
  Local Filesystem actor.

## Use cases

| UC | Name | HTTP | FFI |
| --- | --- | --- | --- |
| UC-38 | Stream file content | `GET /v1/files/{uuid}/stream` | `alexandria_file_playback_source` |
| UC-39 | Read a comic book page | `GET /v1/files/{uuid}/pages/{n}` | `alexandria_comic_page` |
| UC-40 | Get a file thumbnail | `GET /v1/files/{uuid}/thumbnail` | `alexandria_file_thumbnail` |

Full use case specifications — actors, preconditions, postconditions, main
flows, and alternative flows in the established table format — are added to the
Use Case Specification Document as part of the implementation.

## Architecture

A new top-level core module `crates/alexandria-core/src/playback/`, a sibling
of `catalog`, `collections`, `watchlists`, and `reading_lists`, matching the
one-module-per-capability-area convention:

```
playback/
  mod.rs             shared resolve-and-guard; PlaybackSource type
  mime.rs            extension -> MIME, one table, shared by all three
  source.rs          UC-38
  comic_page.rs      UC-39
  thumbnail.rs       UC-40
```

All three use cases begin with the same guard — resolve the UUID through the
existing catalog repository, reject a non-`active` file, reject one whose
`missingAt` is set — so that lives once in `mod.rs` rather than in three
copies.

UC-38's route is `/stream`, not `/content`: `GET /v1/files/{uuid}/content`
is already UC-32's text-content route and `PUT` on the same path is UC-33's
editor. `/stream` also describes the operation more honestly — the response
is a seekable byte stream, not a JSON content document.

HTTP gains one `routes/playback.rs` holding all three handlers. FFI gains
three functions and a `PLAYBACK_*` status set, following the established
per-area convention (`INDEX_*`, `FILE_*`, `COLLECTION_*`), with
`PLAYBACK_OK == 0`.

### Dependency changes

| Change | Rationale |
| --- | --- |
| `tower-http` gains the `fs` feature | `ServeFile` already implements Range, `206`, `416`, `Content-Range`, and conditional requests correctly. Hand-rolling Range parsing would re-implement a solved and easy-to-get-subtly-wrong problem. |
| `image` promoted from dev-dependency to dependency, features `jpeg`, `png`, `webp` | Thumbnail decode and downscale (UC-40 only). |
| `zip` promoted from dev-dependency to dependency | Comic page reads. It is already a real dependency of `alexandria-core`; only the dev-dependency duplication goes away. |
| `base64` added | FFI payloads for UC-39 and UC-40. |
| **No new system dependency** | ffmpeg, already linked for video metadata extraction, supplies the video keyframe. |

## Data flow

**UC-38 (HTTP).** Authenticate via existing middleware → resolve UUID →
assert `active` → reject a set `missingAt` as `Disk` → `stat` the path,
mapping a failed `stat` to `Disk` → derive MIME from the extension → hand
the path to `ServeFile`, which opens it, honours `Range`, and streams. The
guard completes before any byte is written, so every failure is a clean JSON
error response and nothing can fail halfway through a `200`.

The `stat` is load-bearing twice over: it supplies `sizeBytes` for the FFI
descriptor, and it is what turns a file that vanished without a re-index
into a `Disk` error. Without it `ServeFile` would answer its own `404`,
which would tell the client the catalog record does not exist when it
plainly does. Checking `missingAt` first is a cheap short-circuit for the
case re-index already knows about.

**UC-38 (FFI).** The same guard, then `{"path", "mimeType", "sizeBytes"}`.
No bytes cross the boundary.

**UC-39.** Guard → assert `type == comic` → assert the path ends `.cbz` →
open the archive → select and order the page entries (below) → bounds-check
the 1-based `n` → return the entry's raw bytes plus the MIME type derived
from the entry's extension.

**Page selection and ordering.** `comic_tags.rs::read_cbz` already decides
*which* archive entries count as pages: entries whose extension is in
`IMAGE_EXTENSIONS`, with `ComicInfo.xml` excluded. That selection rule is
extracted into a shared helper so the page count and the page index can never
disagree about what a page is.

It does **not** define an order — it counts in archive-index order, which is
sufficient for a count and insufficient for "page N". UC-39 therefore adds
an explicit ordering: **case-insensitive lexicographic sort of the entry
name**. This is what comic readers conventionally do and it is correct for
the zero-padded filenames (`page001.jpg`) that CBZ archives overwhelmingly
use. Archive-storage order is not relied upon, because nothing guarantees a
writer stored the entries in page order. Ordering is order-independent from
counting, so `comicPageCount` remains consistent whichever order is applied.

**UC-40.** Guard → assert the type is video, image, or comic → look up the
cache by `contentHash` + max dimension → on a miss, produce the source image
(video: an ffmpeg keyframe; image: decode the file; comic: decode page 1 via
the UC-39 path) → downscale preserving aspect ratio to fit `maxDim` (default
320) → encode JPEG → write the cache entry → return the bytes.

**MIME resolution.** One table in `mime.rs`, keyed by extension, covering the
formats `classify.rs` already recognizes for each of the seven file types. An
extension absent from the table yields `application/octet-stream` rather than
an error: the bytes are still streamable, and refusing to serve a file the
catalog happily indexed would be inconsistent.

## Configuration

One new key, in the existing config file and `config.toml.example`:

| Key | Default | Meaning |
| --- | --- | --- |
| `playback.thumbnail_cache_dir` | `"thumbnails"` | Directory holding generated thumbnails, created on first use. Relative by default, matching the style of `database.path`. |

The thumbnail's maximum dimension is a compile-time constant (320), not a
config key and not a query parameter — there is one thumbnail size until
something needs a second. The cache key includes it anyway, precisely so that
introducing a second size later cannot collide with entries written under the
first.

## Error handling

No new `DomainError` variants; the existing ones and
`middleware/error.rs` cover every case.

| Condition | DomainError | HTTP | FFI |
| --- | --- | --- | --- |
| UUID unknown | `NotFound` | 404 | `PLAYBACK_ERR_NOT_FOUND` |
| File is `deleted` | `InvalidState` | 409 | `PLAYBACK_ERR_INVALID_STATE` |
| `missingAt` is set, or `stat`/read fails | `Disk` | 500 | `PLAYBACK_ERR_DISK` |
| Wrong type for the operation | `InvalidInput` | 400 | `PLAYBACK_ERR_INVALID_INPUT` |
| CBR comic page requested | `InvalidInput` | 400 | `PLAYBACK_ERR_INVALID_INPUT` |
| Page index out of range | `InvalidInput` | 400 | `PLAYBACK_ERR_INVALID_INPUT` |
| Unsatisfiable Range | handled by `ServeFile` | 416 | n/a |
| Caller not authenticated | `Unauthorized` | 401 | `PLAYBACK_ERR_UNAUTHORIZED` |

Two mappings are deliberate and worth stating:

- **A CBR page request is `InvalidInput`, not `NotFound`.** The file exists
  and is genuinely a comic; what is unsupported is the archive *format*, and
  the error message says so.
- **`missingAt` is a disk error, not a 404.** The catalog record exists and
  is valid; `NotFound` would tell the client something false about its own
  catalog.

## Front-end integration note

These endpoints sit behind the same auth middleware as every other route, so
the Flutter player must send the `Authorization` header on its media request.
`video_player` and `just_audio` both support per-request headers, so this
works — but it is something the front-end must do explicitly rather than get
for free from a bare URL.

## Testing

Per the Testing Specification Document's three tiers, with Given-When-Then
names and Arrange/Act/Assert bodies.

**Unit** — `playback` logic against trait fakes, no filesystem:

- MIME table: a known extension per file type maps to the expected type; an
  unknown extension maps to `application/octet-stream`.
- Shared guard: unknown UUID → `NotFound`; `deleted` → `InvalidState`;
  `missingAt` → `Disk`.
- Type gates: UC-39 against each non-comic type; UC-40 against each
  unsupported type.
- Page bounds: `n = 0`, `n = count + 1`, `n = count`.
- Page ordering: entries supplied out of order sort into page order, and
  the selection rule agrees with the count.

**Integration** — real SQLite, temp filesystem, real fixture files:

- UC-38 full read is byte-identical to the source file.
- Range: a mid-file range returns `206` with the correct `Content-Range` and
  exactly those bytes; an open-ended `bytes=N-` succeeds; a range past EOF
  returns `416`. This is where the seek behaviour a video player depends on
  is actually proven.
- UC-39 against a real CBZ: first, middle, and last page; each returned page
  decodes as a valid image; the page count agrees with the catalog's
  `comicPageCount`.
- UC-39 against a `.cbr` → `InvalidInput`.
- UC-40 for video, image, and comic: the output decodes as JPEG, fits within
  `maxDim`, and preserves the source aspect ratio.
- UC-40 cache: the first call populates the cache directory; the second
  returns identical bytes; a changed `contentHash` produces a different key.

**Parity** — HTTP vs FFI, per FR-MP-06:

- UC-38: the descriptor's `mimeType` and `sizeBytes` match the HTTP
  response's `Content-Type` and `Content-Length`, and the path it returns
  holds bytes identical to what HTTP streamed. This is the strongest
  statement available once the surfaces intentionally differ in transport,
  and it catches the failure that matters — a descriptor pointing somewhere
  other than what HTTP serves.
- UC-39 and UC-40: base64-decoding the FFI payload equals the HTTP body,
  byte for byte.
- All three: every row of the error table produces the corresponding
  `PLAYBACK_ERR_*` and HTTP status from the same input.

The Testing Specification already describes fixtures for the formats needed —
an EXIF JPEG, a CBZ, an ffmpeg-encoded MP4 — so UC-39 and UC-40 reuse them
rather than adding new ones.

**Assumption to verify during implementation:** the CI environment can build
and run against ffmpeg on a *read* request path. The NFR-02 harness already
exercises ffmpeg-based extraction, so the toolchain is established, but UC-40
is the first place ffmpeg runs while serving a request rather than while
indexing.

## Out of scope

- Transcoding or re-encoding of any kind (FR-MP-03, Vision §1.2).
- PDF page rasterization (Decision 1).
- CBR page extraction (Decision 5).
- Resume position and continue-watching (Decision 7) — its own spec.
- Thumbnail cache eviction (Decision 6).
- Audio cover-art thumbnails. FR-MP-05 covers video, image, and comic only.
  `lofty` can read embedded cover art and this would be a small addition, but
  it is a separate decision about what an audio row looks like in the
  front-end, not a gap in this one.
