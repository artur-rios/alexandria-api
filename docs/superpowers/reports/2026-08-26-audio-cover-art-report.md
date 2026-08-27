# Audio cover art — implementation report

**Date:** 2026-08-26
**Branch:** `feature/117-audio-cover-art-thumbnail`
**Issue:** #117
**Design:** `docs/superpowers/specs/2026-08-26-audio-cover-art-design.md`

## Summary

`ThumbnailHandler` gained a `FileType::Audio` arm that extracts the embedded
front-cover picture from an audio file's own tag (via a new `lofty`-backed
port) and hands it to the existing `ImageThumbnailRenderer`, same as every
other arm. Nothing is read or stored at index time; the picture is read
fresh on every uncached request and cached under the same uuid+mtime key
every other thumbnail uses.

## Changes per file

### `crates/alexandria-core/src/catalog/audio_tags.rs`
Added, beside `AudioMetadataReader`/`LoftyAudioMetadataReader`:
- `CoverArtReader` trait — `async fn read(&self, path: &str) -> Option<Vec<u8>>`.
  A sibling of `AudioMetadataReader`, not an extension of it (documented why:
  different read timing, different reasons — index-time prefill vs.
  on-demand thumbnail).
- `LoftyCoverArtReader` — the real implementation. Opens the file with
  `lofty::probe::Probe`, reads `primary_tag().or_else(first_tag)`, picks the
  picture whose `PictureType` is `CoverFront`, falling back to the tag's
  first picture if none is explicitly marked front cover (matches the
  design's "first picture is a better answer than nothing" call).
- The size check is split out as `parse_capped(path, cap)`, with `parse`
  calling it with the real `MAX_PLAYBACK_READ_BYTES` — mirroring
  `playback::read_capped`'s own cap-as-parameter shape, specifically so a
  unit test can drive the over-cap branch with an 8/16-byte fixture instead
  of allocating a 256 MiB one.
- 8 new unit tests on the real reader: front cover returned, back-cover-only
  falls back to "first picture", untagged file → `None`, tagged-but-no-
  picture file → `None`, missing file → `None` (not a panic), over-cap →
  `None`, at-cap → returned.

### `crates/alexandria-core/src/playback/thumbnail.rs`
- `ThumbnailHandler<A, R, C, T, K>` gained a sixth generic parameter, `V:
  CoverArtReader`, and a `cover: V` field, threaded through `new()`.
- New `FileType::Audio` match arm: calls `self.cover.read(&file.path)`; `None`
  becomes `DomainError::InvalidInput` in the same shape the SVG and `.cbr`
  rejections use; `Some(picture)` goes to
  `self.renderer.from_image_bytes(&picture, THUMBNAIL_MAX_DIM)`.
- Updated the catch-all arm's message and the struct/module doc comments to
  say "video, image, comic, and audio" throughout.
- Test module: added `FakeCoverArt` (fixed answer + call log, so a test can
  assert both what the handler did with the result and whether the reader
  was consulted at all). Updated all 10 existing `ThumbnailHandler::new(...)`
  call sites to pass a `FakeCoverArt::none()`. Added 6 new unit tests:
  cover art present → thumbnail rendered from it; no cover art →
  `InvalidInput`; soft-deleted audio file → `InvalidState` *and* the cover
  reader was never called; unauthenticated caller → `Unauthorized` and the
  cover reader was never called; `THUMBNAIL_MAX_DIM` passed to the renderer
  matches every other arm (via a small `DimRecordingRenderer` local to that
  test, since `FakeRenderer` ignores the dimension it's given).

### `crates/alexandria-core/src/services.rs`
- Imported `LoftyCoverArtReader` beside `LoftyAudioMetadataReader`.
- `DefaultThumbnailHandler` type alias gained the sixth type parameter.
- `ThumbnailHandler::new(...)` construction in `build_services` now passes
  `LoftyCoverArtReader` as the last argument, with a comment pointing at why
  it sits next to the audio tag reader's own construction.

### `crates/alexandria-http/Cargo.toml`
- Added `lofty.workspace = true` to `[dev-dependencies]`, with a comment
  mirroring the existing `ffmpeg-next` precedent: the audio thumbnail test
  needs a genuinely tagged fixture, built at test time.

### `crates/alexandria-http/tests/common/mod.rs`
- `write_minimal_wav` (private): a duplicate of `alexandria-core`'s
  `audio_tags` test helper of the same name — a minimal valid RIFF/WAVE
  container, just enough for `lofty` to accept a written tag.
- `write_audio_with_cover_art(dir, name, jpeg) -> PathBuf`: writes the WAV,
  then an ID3v2 tag carrying one `CoverFront` picture built from the caller's
  JPEG bytes.
- `write_audio_without_cover_art(dir, name) -> PathBuf`: the WAV with no tag
  at all — the "genuinely no picture" fixture, distinct from an unparseable
  one.

### `crates/alexandria-http/tests/playback_thumbnail_api.rs`
- Updated the `given_document_when_thumbnailed_then_bad_request` comment to
  say "video, image, comic, and audio."
- 3 new integration tests against the real axum router + real SQLite +
  `/v1/index`:
  - `given_audio_with_cover_art_when_thumbnailed_then_jpeg_returned` —
    indexes a WAV with an embedded cover, requests its thumbnail, asserts
    `200`, `image/jpeg`, and a decodable JPEG body.
  - `given_audio_with_no_cover_art_when_thumbnailed_then_bad_request` —
    indexes an untagged WAV, asserts `400`.
  - `given_audio_thumbnail_requested_twice_then_second_call_is_cached` —
    same caching-behavior assertion the image test already makes (identical
    bytes on both calls, one cache file written), mirrored for audio.

### `crates/alexandria-ffi/tests/parity.rs`
- Added `write_cover_art(path, jpeg)`, reusing the file's existing
  `write_minimal_wav` and mirroring its existing `write_test_tags` helper.
- Added
  `given_same_audio_file_when_thumbnailed_then_bytes_identical_across_surfaces`
  (NFR-09, FR-MP-06), modeled directly on the existing image-thumbnail
  parity test: two independently-seeded tagged WAVs (one per leg, same
  embedded cover), the HTTP route on one leg, `alexandria_file_thumbnail` on
  the other, and an assertion that the returned JPEG bytes, MIME type, and
  decoded dimensions all agree. Uses `seed_file_at_path(..., "audio", ...)`
  directly rather than indexing through `/v1/index`, matching the shortcut
  the existing audio-tag-extraction parity test in the same file already
  takes.
  - Note: the assertion is on decoded pixel dimensions (4×4) and byte
    equality *between the two transports*, not byte equality against the
    original embedded JPEG — the renderer decodes and re-encodes the source
    picture (`image::load_from_memory` → `JpegEncoder`), so the output is a
    different, but equally deterministic-per-input, set of bytes.

### `docs/requirements/System Requirements Document.md`
- FR-MP-05: "for a video, image, or comic File" → "for a video, image,
  comic, or audio File", plus a sentence on what an audio thumbnail is and
  what happens when there is no embedded picture.
- The `GET /v1/files/{uuid}/thumbnail` endpoint-table row's description
  updated to list audio alongside video/image/comic.
- Searched the whole document for other `FR-MP-05` occurrences (the mtime
  field description at line 263 references FR-MP-05 generically and needed
  no change).

### `docs/requirements/Use Case Specification Document.md`
- Not explicitly in scope per your instructions (which named only the SRD),
  but UC-40's own description, precondition, main-flow steps, and AF-01 all
  said "video, image, or comic" / listed audio under "types with no
  thumbnail" — directly contradicted by the new behavior. Updated all four
  to include audio, since leaving them would make the use case doc
  self-contradicting the moment this branch merges. Flagging this as a
  judgment call in case you'd rather that document stay untouched and be
  handled separately.

## Fixture problem — how it was solved

Per the design's own risk note, every fixture is built at test time with
`lofty`, not committed as a binary:
- `alexandria-core`: reuses the existing `write_minimal_wav` (already in
  `audio_tags`'s test module) and writes an ID3v2 tag with
  `Picture::unchecked(...).pic_type(...).mime_type(MimeType::Jpeg).build()`,
  pushed via `Tag::push_picture`.
- `alexandria-http`: duplicated `write_minimal_wav` into `tests/common/mod.rs`
  (it's `#[cfg(test)]`-private in `alexandria-core`, so it can't be
  imported directly), added `write_audio_with_cover_art` /
  `write_audio_without_cover_art` built the same way, and reused the
  existing `jpeg_bytes_for` helper for a genuinely decodable embedded
  picture.
- `alexandria-ffi`: `parity.rs` already had its own `write_minimal_wav` (used
  by the pre-existing audio-tag-extraction parity test); added a sibling
  `write_cover_art` following the same pattern.

No binary fixture was committed anywhere.

## Design points left to my judgment

1. **Fallback picture selection**: the design says "where it carries
   pictures but names none of them the front cover, the first is a better
   answer than nothing." Implemented as: search for `PictureType::CoverFront`
   first; if none, take `pictures().first()`. Covered by
   `given_only_a_back_cover_when_read_then_it_is_returned_anyway`.
2. **Where the cap lives**: rather than capping the *file read* the way
   `read_capped` bounds the image arm's raw file read, the cap here bounds
   the *picture's extracted byte length* after `lofty` has parsed the tag —
   `lofty::probe::Probe` reads the container sequentially rather than
   loading the whole file, so the actual unbounded quantity is the picture
   payload itself, not the read of the file that contains it. Documented
   inline in `LoftyCoverArtReader::parse_capped`.
3. **Generic parameter naming**: added `V` as `ThumbnailHandler`'s sixth type
   parameter (after `A, R, C, T, K`) rather than inserting it earlier, to
   minimize the diff on every existing call site.
4. **Trait placement**: `CoverArtReader` lives in `audio_tags.rs` beside
   `AudioMetadataReader`/`LoftyAudioMetadataReader` (both trait and impl),
   rather than beside `ThumbnailRenderer`/`ThumbnailCache` in `thumbnail.rs`.
   The design's text ("wired in `services.rs`, beside the tag reader it sits
   next to") reads most naturally as "the whole reader — port and impl —
   sits beside the tag reader," and it keeps the `lofty`-specific knowledge
   (picture types, tag structure) in one file with the other `lofty` reader.
5. **Use Case Specification Document** update — see above; flagged rather
   than silently done, since it wasn't named in the explicit instructions.

## Commands run (exact) and outcomes

```
cargo build --workspace                          → exit 0 (2m 49s)
cargo build --workspace --tests                   → exit 0 (7m 57s, first full test-target compile)
cargo fmt --all                                    → reformatted 1 file (thumbnail.rs, one wrapped line)
cargo fmt --all -- --check                         → exit 0, no output (clean)
cargo clippy --workspace --all-targets -- -D warnings
                                                    → first run caught a duplicate test name
                                                      (given_untagged_wav_when_read_then_none used
                                                      twice in audio_tags.rs); fixed by renaming the
                                                      new one to given_untagged_wav_when_cover_read_then_none
                                                    → second run: exit 0, clean (1m 04s)
cargo test --workspace                             → exit 0, 52 "test result: ok" blocks, 0 failures
                                                      across every crate (core lib+integration,
                                                      http integration, ffi integration incl. parity)
```

New tests specifically verified present and passing in the log:
- `catalog::audio_tags::tests::given_front_cover_when_read_then_its_bytes_returned`
- `catalog::audio_tags::tests::given_only_a_back_cover_when_read_then_it_is_returned_anyway`
- `catalog::audio_tags::tests::given_untagged_wav_when_cover_read_then_none`
- `catalog::audio_tags::tests::given_tag_with_no_pictures_when_read_then_none`
- `catalog::audio_tags::tests::given_missing_file_when_cover_read_then_none_not_panic`
- `catalog::audio_tags::tests::given_picture_over_cap_when_parsed_then_none`
- `catalog::audio_tags::tests::given_picture_exactly_at_cap_when_parsed_then_returned`
- `playback::thumbnail::tests::given_audio_with_cover_art_when_thumbnailed_then_picture_rendered`
- `playback::thumbnail::tests::given_audio_with_no_cover_art_when_thumbnailed_then_invalid_input`
- `playback::thumbnail::tests::given_deleted_audio_file_when_thumbnailed_then_reader_not_consulted`
- `playback::thumbnail::tests::given_unauthenticated_caller_when_audio_thumbnailed_then_reader_not_consulted`
- `playback::thumbnail::tests::given_audio_with_cover_art_when_thumbnailed_then_max_dim_matches_other_arms`
- `given_audio_with_cover_art_when_thumbnailed_then_jpeg_returned` (alexandria-http)
- `given_audio_with_no_cover_art_when_thumbnailed_then_bad_request` (alexandria-http)
- `given_audio_thumbnail_requested_twice_then_second_call_is_cached` (alexandria-http)
- `given_same_audio_file_when_thumbnailed_then_bytes_identical_across_surfaces` (alexandria-ffi, parity)

## Concerns

- None blocking. The one open item is the Use Case Specification Document
  edit noted above — done for consistency, but it goes beyond the literal
  instruction, so flagging it explicitly.
- Did not open a PR, did not merge, did not touch `main` — commit only, on
  `feature/117-audio-cover-art-thumbnail`, as instructed.

## Addendum — review fixes (10 findings)

A review pass came back with 10 findings against the first version above.
All 10 are addressed.

### FINDING 1/2 (Important) — the read was unbounded; the doc comment claimed otherwise

The original `parse_capped` only compared the *extracted picture's* length
against `cap`, after `Probe::read()` had already parsed the whole tag —
`lofty`'s ID3v2 frame reader allocates a frame's *declared* size into an
owned `Vec` before it has read that many bytes back off disk, for every
picture in the tag, not just the one selected. The check ran after the
allocation it meant to prevent.

Fixed in `crates/alexandria-core/src/catalog/audio_tags.rs`:
`LoftyCoverArtReader::parse_capped` now stats the file with
`std::fs::metadata` and rejects (`CoverArtRead::Unreadable`) anything larger
than `cap` before calling `Probe::open`/`.read()` at all. `lofty` 0.25's
`ParseOptions` has no per-frame or per-picture size limit (checked —
`read_cover_art` is a bool, not a bound), so this file-size precheck is the
best available bound, not a complete one: a maliciously small file could
still declare an inflated frame size and cost a transient allocation before
`read_exact` fails on it, bounded only by the format's own ~4 GiB frame-size
ceiling. The doc comment on `parse_capped` now says exactly this — what is
bounded, what residual risk remains, and why no better option exists in this
lofty version — instead of asserting full protection the code did not
provide. The original post-parse picture-length check is kept as a backstop
(now via `exceeds_cap`, see below), though it is provably unreachable through
`parse_capped` for any real file once the precheck uses the same cap (a
picture's bytes are a subset of the file's own, so a file within `cap` can
never contain a picture over it) — the doc comment says this too.

### FINDING 3 (Important) — `docs/System Behavior Document.md:811` missed

Updated the `GET /v1/files/{uuid}/thumbnail` section (6.3) to list audio
alongside video/image/comic, and added a paragraph describing the audio
front-cover behavior and, per Finding 4's resolution, the 400-vs-500 split
between "no picture" and "unreadable."

### FINDING 4 (Important) — AF-04 was false for audio; chose to distinguish read failure from absent picture

Chose the second option the review offered (make `CoverArtReader`
distinguish a read failure from an absent picture), not a documentation-only
carve-out, because the code-level distinction stayed entirely within
`LoftyCoverArtReader` and cost no extra I/O.

- `CoverArtReader::read` now returns a new enum, `CoverArtRead { Found(Vec<u8>),
  NoPicture, Unreadable }`, replacing `Option<Vec<u8>>`.
- `LoftyCoverArtReader::parse_capped` returns `Unreadable` for: oversized
  file (the new precheck), `std::fs::metadata` failure, and
  `Probe::open`/`.read()` failure (missing/corrupt/unsupported format).
  Returns `NoPicture` for: no tag at all, tag with no pictures, or a picture
  over the post-parse cap.
- `ThumbnailHandler`'s `FileType::Audio` arm now matches on the three
  variants: `Found` renders as before; `NoPicture` is `InvalidInput` (400,
  unchanged behavior); `Unreadable` is now `DomainError::disk(...)` (500),
  matching the classification `video_tags`/the video thumbnail arm already
  give an undecodable source.
- `docs/requirements/Use Case Specification Document.md` UC-40's AF-04 text
  amended to explicitly carve out audio's "could not be opened or parsed at
  all" case, told apart from AF-01's "parsed fine, no picture."
- New unit test `given_unreadable_audio_when_thumbnailed_then_disk_error` in
  `thumbnail.rs`, plus `FakeCoverArt::unreadable()`.
- Real-reader tests split accordingly:
  `given_missing_file_when_cover_read_then_unreadable_not_panic` and the new
  `given_garbage_bytes_when_cover_read_then_unreadable_not_panic` assert
  `Unreadable`; `given_untagged_wav_when_cover_read_then_no_picture` and
  `given_tag_with_no_pictures_when_read_then_no_picture` assert `NoPicture`.

### FINDING 5/6 (Minor) — bad citation and wrong claim in `playback_thumbnail_api.rs`

- Dropped the false "Testing Specification's own audio-thumbnail
  requirement" citation from the caching test's comment (replaced by an
  explanation of what the test actually proves, see Finding 9).
- Fixed the "returns a source already inside the box untouched" comment on
  `given_audio_with_cover_art_when_thumbnailed_then_jpeg_returned` — the
  renderer always re-encodes through `JpegEncoder`; the comment now says
  "re-encoded at its own size, never enlarged" and states the assertion is
  "decodes to a valid JPEG," not "byte-identical to the source."

### FINDING 7 (Minor) — `CoverFront` preference untested

Added `given_front_cover_after_another_picture_when_read_then_front_cover_wins`
in `audio_tags.rs`: a `CoverBack` picture written first, a `CoverFront`
second, asserting the front cover is what comes back — the one test that
actually exercises the `find(CoverFront)` branch rather than only its
`.first()` fallback. Added `write_pictures` (plural) as the underlying
fixture helper; `write_picture` (singular) now delegates to it.

### FINDING 8 (Minor) — no test for genuinely unparseable bytes

Added `given_garbage_bytes_when_cover_read_then_unreadable_not_panic`: a real
file on disk, small enough to pass the size precheck, whose contents are not
a RIFF/WAVE container at all — fails inside `Probe::read`'s parse, not at
the file-open step the missing-file test already covers.

### FINDING 9 (Minor) — caching test did not actually prove caching

Renamed to
`given_audio_thumbnail_requested_twice_then_second_call_reads_from_cache` and
rewrote it to delete the source WAV file between the two requests. A cache
miss on the second call would now surface as a disk error (the file is
gone); the test asserts `200` with the first call's exact bytes, which can
only come from the cache.

### FINDING 10 (Minor) — wrong `#[tokio::test]` and an inaccurate doc comment

- The two picture-cap tests that never awaited anything
  (`given_picture_over_cap_when_parsed_then_none`,
  `given_picture_exactly_at_cap_when_parsed_then_returned`) are gone; the
  logic they tested moved into a tiny sync `LoftyCoverArtReader::exceeds_cap`
  helper (necessary anyway, since the file-size precheck makes the full
  `parse_capped` path unreachable for that branch — see Finding 1/2), tested
  by two new `#[test]` (non-async) functions:
  `given_picture_length_over_cap_then_exceeds_cap_true` and
  `given_picture_length_at_cap_then_exceeds_cap_false`.
- `jpeg_bytes_for`'s doc comment in `audio_tags.rs` rewritten to state only
  what is true: local to the module because it takes a `u8` seed rather than
  the HTTP suite's `&str`; dropped the "so a test can recompute the exact
  bytes" and "mirrors" claims, neither of which held.

## Commands run and outcomes (review-fix pass)

```
cargo build --workspace --tests          -> exit 0 (15m 01s)
cargo clippy --workspace --all-targets -- -D warnings
                                           -> exit 0, clean (2m 02s)
cargo fmt --all                           -> reformatted 1 file (audio_tags.rs)
cargo fmt --all -- --check                -> exit 0, clean
cargo test --workspace                    -> exit 0, 52 "test result: ok" blocks, 0 failures
```

New/renamed tests confirmed present and passing in the log, including:
`given_picture_length_at_cap_then_exceeds_cap_false`,
`given_picture_length_over_cap_then_exceeds_cap_true`,
`given_missing_file_when_cover_read_then_unreadable_not_panic`,
`given_unreadable_audio_when_thumbnailed_then_disk_error`,
`given_garbage_bytes_when_cover_read_then_unreadable_not_panic`,
`given_untagged_wav_when_cover_read_then_no_picture`,
`given_file_larger_than_cap_when_parsed_then_unreadable_before_parsing`,
`given_file_exactly_at_cap_when_parsed_then_parsed_normally`,
`given_front_cover_when_read_then_its_bytes_returned`,
`given_only_a_back_cover_when_read_then_it_is_returned_anyway`,
`given_front_cover_after_another_picture_when_read_then_front_cover_wins`,
`given_tag_with_no_pictures_when_read_then_no_picture`,
`given_audio_thumbnail_requested_twice_then_second_call_reads_from_cache`,
`given_same_audio_file_when_thumbnailed_then_bytes_identical_across_surfaces`
(FFI parity, unaffected by the enum change since it exercises the real HTTP
and FFI surfaces rather than the port directly).

## Concerns (updated)

None blocking. The residual gap named in Finding 1/2's fix — a small,
maliciously crafted file with an inflated declared frame size can still cost
a transient allocation up to `lofty`'s own per-frame ceiling before failing
— has no further mitigation available without forking `lofty` or writing a
custom ID3v2/Vorbis/MP4 parser; documented in code rather than silently
accepted.
