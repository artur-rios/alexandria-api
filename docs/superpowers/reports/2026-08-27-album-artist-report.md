# The album artist — implementation report

**Date:** 2026-08-27
**Branch:** `feature/120-album-artist`
**Issue:** [#120](https://github.com/artur-rios/alexandria-api/issues/120)
**Design:** `docs/superpowers/specs/2026-08-27-album-artist-design.md`

## Summary

An audio file now carries a seventh extracted field, `album_artist` —
`ALBUMARTIST` (Vorbis comments), `TPE2` (ID3v2), `aART` (MP4 atoms) — read at
first index by `LoftyAudioMetadataReader`, stored on `audio_files`, exposed on
`SubtypeMetadata::Audio` over both HTTP and FFI, and editable/clearable
through UC-04 (FR-FC-14). The core never falls back to the track artist: an
absent tag reads `albumArtist: null`, never a copy of `artist`.

## lofty accessor used

`lofty::tag::Tag::get_string(ItemKey::AlbumArtist)` — the same direct
item-key lookup `year_of` already uses for `ItemKey::Year`/`RecordingDate`,
since `lofty::tag::Accessor` has no dedicated `album_artist()` convenience
the way it has `title()`/`artist()`/`album()`/`genre()`. `ItemKey::AlbumArtist`
is lofty's own unification of the three format-specific tags (confirmed by
reading `lofty-0.25.1/src/tag/item.rs`'s key-mapping tables):

- ID3v2 (MP3, WAV, AIFF): `TPE2`
- Vorbis comments (FLAC, OGG/OGA, Opus): `ALBUMARTIST` / `Album Artist`
- MP4 atoms (M4A, AAC-in-MP4): `aART`

Verified by two round-trip unit tests in `audio_tags.rs` that write an ID3v2
tag via `lofty` itself (the existing fixture pattern —
`write_test_tags`/`write_minimal_wav`), one with the album-artist item set,
one without (with every other field still set), both read back through the
real `LoftyAudioMetadataReader`. Vorbis/MP4 are not separately
fixture-tested — no existing test in this file writes those container
formats either, and `ItemKey::AlbumArtist` is read uniformly regardless of
the tag type the file's own primary tag turns out to be, so the ID3v2 round
trip exercises the same code path.

## Files changed

### Domain core

- **`crates/alexandria-core/migrations/00000000000015_album_artist.sql`** —
  new. `ALTER TABLE audio_files ADD COLUMN album_artist TEXT;`, following
  `00000000000010_video_duration.sql`'s precedent (nullable `ADD COLUMN`,
  no backfill).
- **`crates/alexandria-core/src/catalog/audio_tags.rs`**
  - `AudioTags` gains `album_artist: Option<String>`, documented with the
    design's own no-fallback rationale.
  - `into_subtype_metadata` includes it in the all-`None` check and the
    `SubtypeMetadata::Audio` construction.
  - `LoftyAudioMetadataReader::parse` reads it via `get_string` (see above),
    filtered through the same empty/whitespace-only-string-becomes-`None`
    rule the other string fields already use.
  - Unit tests added/extended: `into_subtype_metadata` with only
    `album_artist` set; a tagged-WAV round trip with the field present; a
    tagged-WAV round trip with every *other* field present but this one
    absent (the design's named case — "one carrying none returns `None`
    for that field while still returning the others"); the existing
    blank-string-tags test extended to cover an empty-string album-artist
    item. The existing missing-file/unparseable-file test already covers
    "unparseable file still returns nothing at all" generically for every
    field, this one included.
- **`crates/alexandria-core/src/catalog/model.rs`** — `SubtypeMetadata::Audio`
  gains `album_artist: Option<String>` with `#[serde(rename = "albumArtist",
  skip_serializing_if = "Option::is_none")]`, matching the `mediaKind`
  precedent for a multi-word field name (all six pre-existing Audio fields
  are single words, so none of them carry an explicit `rename`).
- **`crates/alexandria-core/src/catalog/repos.rs`** — every place audio
  columns were named, in order:
  1. `batch_audio`'s `SELECT` (issue #116's batched listing query) and its
     row-tuple destructure/all-`None` check/`SubtypeMetadata::Audio` build.
  2. The `AudioRow` and `AudioBatchRow` type aliases (both grew a seventh
     `Option<String>`).
  3. `update_metadata`'s `UPDATE audio_files SET …` arm (FR-FC-14's writer).
  4. `find_metadata_by_uuid`'s `SELECT` (the single-file read) and its
     destructure/all-`None` check/`SubtypeMetadata::Audio` build.
  - The `INSERT INTO audio_files (file_id) VALUES (?)` in
    `insert_subtype_sql` needed no change — it inserts only the FK, every
    other column (album_artist included) defaults to `NULL`.

### HTTP / FFI test surfaces (parity, FR-FC-24 / NFR-09)

- `crates/alexandria-http/tests/catalog_api.rs` — `AudioMetadataRow` grew a
  seventh column; the existing PATCH-audio-metadata test now sets and
  asserts `albumArtist` end to end (response JSON + persisted row); a new
  test proves a second PATCH that omits `albumArtist` clears the
  previously-set value to `NULL` rather than leaving it stale; the
  five-subtypes batch-query regression test (code-review Finding 1's
  guard against a wrong hard-coded column name) now also sets and asserts
  `album_artist` for the audio case.
- `crates/alexandria-ffi/tests/smoke.rs` — same `AudioMetadataRow`/edit-test
  extension on the FFI side.
- `crates/alexandria-ffi/tests/parity.rs` — `AudioMetadataRow` grew a
  seventh column, both raw `SELECT`s that read it back; the HTTP↔FFI
  edit-metadata parity test's patch body now includes `albumArtist` so the
  parity assertion actually exercises the new field, not just the six
  pre-existing ones.
- `crates/alexandria-core/tests/catalog/{browse,edit_metadata,index}.rs`,
  `crates/alexandria-core/tests/browse_batching.rs` — every existing
  `SubtypeMetadata::Audio`/`AudioTags` struct literal updated for the new
  field (compile requirement); the index-handler happy-path test and one
  edit_metadata test now set it to a real value rather than leaving it
  `None` everywhere, so at least one handler-level test per Testing
  Specification §3 actually carries data through the field, not just
  compiles against it.
- **New**: `edit_metadata.rs` —
  `given_audio_file_with_album_artist_when_cleared_then_none_persisted`,
  a handler-level set-then-clear test (FakeCatalogRepository) mirroring the
  HTTP-level one.

### The risk the design calls out by name

- **New**: `crates/alexandria-core/tests/migrations.rs` —
  `given_a_pre_15_audio_row_when_migrated_then_album_artist_reads_null_not_missing_data`.
  Applies migrations 0…14 by hand (the same subset-`Migrator` technique the
  existing migration-14 test uses), inserts a `files` row and an
  `audio_files` row through *that* schema (no `album_artist` column exists
  yet to write to — this is the only INSERT a pre-branch install could ever
  have run), then lets `run_migrations` apply migration 15 on top of
  pre-existing data. Asserts the row's five other populated columns survive
  untouched and `album_artist` reads `None` through **both** of the
  repository's hard-coded audio `SELECT`s — `find_metadata_by_uuid` (single
  file) and `list_filtered_view`/`batch_audio` (the batched listing) — since
  a mistake isolated to only one of the two would pass a test that checked
  just the other.

### Requirements / behavior docs

- **`docs/requirements/System Requirements Document.md`**
  - FR-FC-14: field list gains `albumArtist`.
  - §4.2 subtype-fields table: `AudioFile` row gains `albumArtist` (this is
    what FR-FC-01 references as "what an audio file's record holds"; FR-FC-01's
    own prose lists no fields today — none of FR-FC-02/05/06's sibling
    entries changed shape for their own added fields either, they name
    fields inline while FR-FC-01 does not — so the concrete edit is the
    table, not FR-FC-01's requirement text itself).
- **`docs/requirements/Technology Stack Document.md`** — the `lofty` row's
  field list gains `albumArtist`.
- **`docs/System Behavior Document.md`** — §5.12's Audio fields list gains
  `albumArtist`; a new paragraph states the no-fallback rule explicitly
  (mirroring how the same section already calls out `mediaKind` and
  `caption` as never inferred — this is a related but distinct kind of
  "not inferred": `albumArtist` *is* extracted, it is just never derived
  from `artist`).
  - Checked the Use Case Specification Document's UC-01 and UC-04: neither
    enumerates the audio field list (unlike the SRD/SBD tables above), so
    nothing there became stale and the traceability table needed no new
    row — both use cases already trace to FR-FC-01/FR-FC-25 (UC-01) and
    FR-FC-14 (UC-04), which already cover this field as one more of the
    subtype's editable columns.

## Commands run

The coordinator ran a first full verification pass independently (`cargo
fmt`, `cargo clippy --workspace --all-targets -D warnings`, `cargo test
--workspace`, all clean) against the state committed as `af00668`, before
review returned five findings (below). After addressing them I re-ran the
same three commands myself against the amended tree:

- `cargo fmt --all` — clean, no reformatting needed.
- `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- `cargo test --workspace` — green, full workspace (`alexandria-core`,
  `alexandria-http`, `alexandria-ffi`).

So: the coordinator ran the first pass on the pre-review commit; I ran the
second pass myself on the post-review amendments before committing them.

## Open questions the design left to this implementation

- **Which lofty read path to use.** The design says "`lofty`… reads all
  three [tag formats]" but doesn't name an accessor. Confirmed no
  `Accessor::album_artist()` convenience exists in lofty 0.25 (unlike the
  other five string/int fields) by reading the crate's own
  `tag/accessor.rs`; used the same direct-`ItemKey` pattern `year_of`
  already established for exactly this situation (`ItemKey::Year` has no
  accessor either).
- **Field naming across the wire.** Chose `albumArtist` (camelCase) for
  JSON, consistent with the codebase's existing convention for every other
  multi-word subtype field (`mediaKind`, `formatKind`, `issueNumber`,
  `comicPageCount`, `durationSeconds`); the design didn't spell out the
  wire name.
- **Where to put the migration-risk test.** The design names the risk but
  not a location. Testing Specification §3 puts repository behavior in
  `alexandria-core/tests/` (sqlite feature); chose `tests/migrations.rs`
  specifically (over `browse_batching.rs` or a new file) because it already
  holds the one other "seed data before a later migration, then apply it"
  test (migration 14's credential-row survival test) and the same
  subset-`Migrator` technique applies directly.

## Review findings addressed (round 1)

1. **Important — extraction parity never carried an album artist.**
   `write_test_tags` in `alexandria-ffi/tests/parity.rs` (the fixture behind
   `given_tagged_audio_file_when_indexed_via_http_and_ffi_then_extracted_metadata_matches`)
   now sets `ItemKey::AlbumArtist`, and that test explicitly asserts
   `albumArtist` on both legs rather than relying only on the whole-body
   `assert_eq!` (a field both sides omit passes that check silently — how
   the gap got through the first time). Added a companion test,
   `given_untagged_album_artist_when_indexed_via_http_and_ffi_then_both_report_null`,
   covering the design's other half: a file with its other six tags present
   but no album-artist item, asserting both surfaces agree it's absent.
2. **Important — placeholder report sections.** Filled in above ("Commands
   run") and this section.
3. **Minor — design/test contradiction on clearing.** The design's testing
   bullet now says what the code actually guarantees ("a later PATCH that
   omits it overwrites the stored value with `NULL` rather than being a
   no-op"), not a tri-state claim. Dropped the "reach the same state
   deliberately" sentence from `catalog_api.rs`'s test doc comment so it no
   longer argues a distinction the implementation doesn't make.
4. **Minor — FR-FC-01 vs the §4.2 table.** Design's requirements-impact
   bullet now names the System Requirements Document's §4.2 subtype-fields
   table directly, rather than FR-FC-01 (whose own prose enumerates no
   fields and so never became stale).
5. **Minor — the first-index-only consequence lived only in the design.**
   Added one sentence to `docs/System Behavior Document.md` (§5.12, after
   the no-fallback paragraph): a library indexed before this field existed
   gains no `albumArtist` until re-indexed afresh, the same consequence
   `duration_seconds` had.

## Concerns

None outstanding. The one open risk the design named by name — pre-existing
rows reading null after the migration — is covered by
`tests/migrations.rs`'s dedicated test through both repository read paths.
The parity gap review caught (Finding 1) is now closed with an explicit,
non-vacuous assertion plus a dedicated "does not have one" test.
