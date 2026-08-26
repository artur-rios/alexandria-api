# Metadata in the catalog listing — implementation report

**Date:** 2026-08-26
**Branch:** `feature/116-metadata-in-catalog-listing`
**Issue:** [#116](https://github.com/artur-rios/alexandria-api/issues/116)
**Design:** `docs/superpowers/specs/2026-08-26-metadata-in-listing-design.md`

## Summary

`GET /v1/files` and `alexandria_files_list` now answer an array of `FileView`
— the same record the single-file call (`GET /v1/files/{uuid}` /
`alexandria_file_get_by_uuid`) already answered — instead of the bare
`File`. The listing is assembled with a bounded number of SQL queries
(the filtered query plus one further query per subtype table the result
actually contains), not one further query per row.

## Files changed

### Domain core

- **`crates/alexandria-core/src/catalog/repos.rs`**
  - Added `MAX_SQLITE_PARAMS: usize = 900` — the assumed SQLite
    bound-parameter ceiling (see "Chunking" below).
  - Added `CatalogRepository::list_filtered_view` to the trait: same
    filter signature as `list_filtered`, returns `Vec<FileView>`.
  - Implemented it on `SqliteCatalogRepository`:
    - Runs the existing filter-building logic (copied from
      `list_filtered`, extended to also `SELECT id`, since the subtype
      tables key on the internal integer id, not the public uuid).
    - Groups the resulting internal ids by `FileType` into five buckets
      (Audio/Video/Document/Comic/Image; Text/Html never batch since they
      carry no `SubtypeMetadata` and no extracted scalar).
    - Runs one batch query per non-empty bucket via five new private
      helpers (`batch_audio`, `batch_video`, `batch_document`,
      `batch_comic`, `batch_image`), each `SELECT`ing every editable
      metadata column **and** that type's extracted scalar in a single
      query per chunk (`width`/`height` for images, `page_count` for
      documents, `duration_seconds` for video, `comic_page_count` for
      comics) — one query does the job `get_by_uuid` needs two separate
      repo calls for, because a batch fetching many rows at once has no
      reason to pay for a second query when both live in the same
      subtype table.
    - Stitches each file to its own batch's row in memory and returns
      `Vec<FileView>`.
  - Added `FileRowWithId` and five `*BatchRow` type aliases alongside the
    existing `FileRow`/`AudioRow`/etc. aliases.
  - Added `SqliteCatalogRepository::in_placeholders` — builds a
    `?,?,...` placeholder list sized to one chunk.

- **`crates/alexandria-core/src/catalog/queries/browse.rs`**
  - `BrowseFilesHandler::list` now returns `Result<Vec<FileView>, DomainError>`
    and calls `repo.list_filtered_view(...)` instead of `repo.list_filtered(...)`.
    The handler itself does no per-row fan-out — all batching lives in the
    repository, where the SQL specifics belong.

- **`crates/alexandria-core/src/playback/test_support.rs`**
  - `FakeRepo` (playback's single-method fake) gained an `unimplemented!()`
    stub for the new trait method, consistent with its existing pattern.

### HTTP / FFI surfaces (both change together, per FR-FC-24 / NFR-09)

- **`crates/alexandria-http/src/routes/browse.rs`** — `list_files` now
  returns `Json<Vec<FileView>>` instead of `Json<Vec<File>>`. Doc comment
  updated; unused `File` import removed.
- **`crates/alexandria-ffi/src/lib.rs`** — `alexandria_files_list`'s
  behavior is unchanged (it already just serializes whatever
  `BrowseFilesHandler::list` returns), only its doc comments and the
  `FileJsonResult` doc comment were updated to describe the new array
  element shape.

### Tests

- **Unit (handler, against `FakeCatalogRepository`)** —
  `crates/alexandria-core/tests/catalog/browse.rs`:
  - Updated every existing `list` test to read through `.file.*` instead
    of the old flat fields (the type changed from `Vec<File>` to
    `Vec<FileView>`).
  - Added three new tests: a single-type listing carries each row's own
    metadata; a mixed-type listing carries each file's own metadata (not
    another file's); a file with no stored metadata carries `None` rather
    than failing.
  - `FakeCatalogRepository::list_filtered_view` (in
    `crates/alexandria-core/tests/common/mod.rs`) was added, assembling
    each `FileView` from the fake's existing in-memory maps — it has no
    queries to batch, so it only needs to prove the right *shape*, not the
    right *query count* (that's the integration test's job). Also added
    the corresponding `unimplemented!()` stub to `FailingCatalogRepository`.

- **Integration (repository, against real SQLite)** — new top-level file
  `crates/alexandria-core/tests/browse_batching.rs` (see "Query-count
  pinning" below for why it's a standalone binary, not folded into
  `tests/catalog.rs`). Two tests:
  - `given_single_type_listing_when_listed_then_query_count_is_bounded_not_per_row`
    — lists 5 then 200 audio files, asserts the query count is identical
    between the two runs (proving it doesn't scale with row count) and
    `<= 3`.
  - `given_mixed_type_listing_when_listed_then_query_count_scales_with_types_present_not_rows`
    — 50 audio files + 1 video file, asserts the query count is exactly 3
    (files query + audio batch + video batch), regardless of the 50:1 skew.

- **Integration (HTTP)** — `crates/alexandria-http/tests/catalog_api.rs`:
  - Fixed the four existing `GET /v1/files` assertions that read top-level
    fields (`arr[0]["state"]`, `arr[0]["name"]`, etc.) to read through
    `arr[0]["file"][...]` instead.
  - Added `given_indexed_audio_file_with_metadata_when_get_files_then_array_element_carries_it`
    — writes audio metadata directly, lists, and asserts the array
    element's `metadata` sub-object carries it.
  - Added `given_indexed_text_file_when_get_files_then_array_element_metadata_is_null`
    — a text file's listing element has `metadata: null`, matching
    `GET /v1/files/{uuid}`'s existing behavior for Text/Html.

- **Parity** — `crates/alexandria-ffi/tests/parity.rs`:
  - Fixed the `contentHash`-null parity test's read of the HTTP listing
    body (`f["name"]`/`["contentHash"]` → `f["file"]["name"]`/
    `["file"]["contentHash"]`).
  - Fixed the list-parity test's `norm` closure (both surfaces, 3-way
    filter comparison) to read `f["file"]["name"]` /
    `f["file"]["fileType"]` / `f["file"]["state"]` instead of the old
    top-level fields.
  - Confirmed every other `/v1/files` reference in this file (unauthorized
    checks, malformed-filter checks, the Windows-login gated-route check,
    the raw `alexandria_index_files_json` accessor comparison) only
    inspects status codes or a wholly separate accessor, so nothing else
    needed changing.

## Batching and chunking

**Assumed limit:** `SQLITE_MAX_VARIABLE_NUMBER` varies by how the linked
SQLite was compiled — 999 on the conservative default still common in
the wild, 32766 on builds using SQLite's newer default. This crate does
not control that at build time, so `MAX_SQLITE_PARAMS` is set to **900**
— comfortably under the lower, older limit — rather than trying to detect
the host's actual ceiling.

**Chunking:** each `batch_*` helper calls `ids.chunks(MAX_SQLITE_PARAMS)`
and issues one `WHERE file_id IN (…)` query per chunk, so a subtype with
more matching ids than the limit costs more queries for *that subtype*
specifically, but the query count still scales with the number of
*chunks*, not the number of *rows* — a library of 10 million audio files
costs the same number of audio-batch queries as a library of 899.

**Resulting query cost** (per the design's own accounting, verified by
the integration test):
- A listing filtered to one type: **2 queries** (files + one subtype
  batch) up to `MAX_SQLITE_PARAMS` ids of that type, or 1 + ⌈n/900⌉
  beyond that.
- An unfiltered/mixed listing: 1 (files) + one query per subtype actually
  present, bounded by 5 (Audio/Video/Document/Comic/Image — Text/Html
  never register).

## Pinning the query count

The design calls out that this must be *asserted*, not trusted. The
mechanism: `sqlx-core` emits a `tracing` event at target `"sqlx::query"`
for every statement it executes. A custom `tracing_subscriber::Layer`
counts these events; before/after diffing the counter around a
`list_filtered_view` call gives an exact query count for that call.

Getting this right took one false start worth recording. The first
attempt installed the counting subscriber as a **thread-local** default
(`tracing::dispatcher::set_default`), scoped to just the calling test's
`.await`. It silently counted **zero** — `cargo test --workspace` caught
it as a hard failure in the mixed-type test (`assert_eq!(queries, 3)`,
got 0), and would *not* have been caught by the single-type test's looser
`<= 3` / `small == large` assertions, since 0 satisfies both trivially.
The cause: sqlx's SQLite driver runs actual statement execution on a
**dedicated worker thread per connection** (SQLite's C API is
synchronous), so the tracing event fires on that worker thread, not on
the thread that awaited the query — a thread-local dispatcher on the
calling thread never sees it.

The fix: install the counter as the **process-global** default
subscriber instead (visible from every thread), and move the two tests
into their own **standalone top-level test file**
(`crates/alexandria-core/tests/browse_batching.rs`, its own `cargo test`
process) rather than folding them into the ~300-test `catalog` binary —
a global counter shared with that many concurrently-running SQLite-touching
tests would count their queries too. The file's own two tests hold a
`static SERIAL: Mutex<()>` across their whole body (the same pattern
`alexandria-ffi/tests/parity.rs` already uses for its own concurrency
concerns) so they never race each other either. With both fixes, the
counts are exact and the mixed-type test's `assert_eq!(queries, 3)` is a
real, non-trivial assertion.

## Documentation amended

- **`docs/requirements/System Requirements Document.md`**
  - **FR-FC-12** rewritten: now states the listing answers the `FileView`
    shape (File + subtype metadata + extracted scalars) and that
    assembly costs a bounded number of queries (filtered query + one per
    subtype table present, chunked for `IN` lists past the bound-parameter
    ceiling) rather than one per file.
  - The `GET /v1/files` row in the HTTP endpoint table (§5.2) updated to
    note it now returns the same record the single-file endpoint does.
  - No traceability-table change was needed: existing FR-FC-12 references
    (§F-02 traceability, UC-03/UC-14 rows) cite the requirement number,
    not its prose, so they stay accurate as-is.
- **`docs/requirements/Use Case Specification Document.md`** — checked;
  UC-03's main-flow step 3 ("The system returns the matching file(s) with
  their metadata") already described the post-change behavior accurately
  and needed no edit.
- **`docs/System Behavior Document.md`** — checked for listing-shape
  claims; its `/v1/files` mentions are all about streaming/paging/
  thumbnail routes, unrelated to this change. No edit needed.

## Commands run (final, clean state)

```
cargo fmt --all                                          # no changes on final pass
cargo clippy --workspace --all-targets -- -D warnings     # 0 warnings
cargo test --workspace                                    # 53 test binaries, all "ok", 0 failed
```

Notably: `alexandria-core`'s `catalog` binary — 297 passed (includes the
updated/added `browse.rs` unit tests); the new standalone
`browse_batching` binary — 2 passed; `alexandria-http`'s `catalog_api`
binary — 97 passed (includes the two new listing-metadata tests);
`alexandria-ffi`'s `parity` binary — 67 passed (includes the fixed
list-parity and hash-parity tests).

## Things the design left open, and decisions made

- **Where batching logic lives**: the design describes the queries at a
  conceptual level; I put all of it in a new `CatalogRepository::
  list_filtered_view` trait method rather than having the handler
  orchestrate five extra repo calls per listing. This keeps SQL-specific
  concerns (the `IN` clause, chunking, the bound-parameter limit) inside
  the repository, matching where every other SQL-specific decision in
  this file already lives, and keeps `BrowseFilesHandler::list` a
  one-line delegation exactly like it was before.
- **Combining metadata and extracted-scalar queries per subtype**: the
  design's accounting lists "audio/video/document/comic/image" and "the
  extracted scalars" as if they might be separate query lines. Since both
  live in the same subtype table and key on the same `file_id`, I fetch
  both in one `SELECT` per subtype per chunk rather than two — strictly
  fewer queries than a literal reading of the design's line-by-line
  accounting, and still satisfies "two or three queries whatever its
  size" for a single-type listing (it lands on exactly two).
- **Chunk size**: the design says "chunked as needed" without a number;
  I picked 900, reasoning documented above and inline on the
  `MAX_SQLITE_PARAMS` constant.

## Concerns

- The 900-id chunk size is untested against SQLite's actual compiled-in
  limit on this build (no test seeds >900 files — that would be a slow
  integration test for marginal additional confidence, since the chunking
  logic itself is exercised structurally and the `IN`-clause construction
  is straightforward `?`-per-id). If a future indexing run ever needed to
  confirm the literal SQLite build's ceiling, that's a one-off `PRAGMA` or
  compile-time check, not a per-run cost.
- The FFI list-parity test (`given_same_lib_when_files_listed_via_http_and_ffi_then_arrays_identical`)
  still only compares `name`/`fileType`/`state` after the fix — it does
  not additionally assert the `metadata` sub-object matches across
  surfaces for that specific test. Cross-surface metadata parity is
  covered elsewhere (the UC-04 edit-metadata parity test already asserts
  `http_body["metadata"] == ffi_body["metadata"]` byte-for-byte after a
  PATCH), and both surfaces reuse the identical `BrowseFilesHandler::list`
  → `Vec<FileView>` → `serde_json::to_string`, so a metadata-shape
  divergence between transports is structurally not possible here — but a
  reviewer preferring belt-and-braces might want an explicit assertion in
  the listing parity test too.
