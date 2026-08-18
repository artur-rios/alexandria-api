# Testing Specification Document — Alexandria

## 1. Purpose

This document defines **how a use case is tested once it has been implemented**.
It is a standard to be followed by any human or agent that builds tests for this
project, so that every use case in the
[Use Case Specification Document](Use%20Case%20Specification%20Document.md)
receives the same shape of testing, with the same tools, naming, and structure.

The rule is simple:

> **After a use case is developed, tests are built for it in the same change —
> before it is considered done.** A use case without its tests is incomplete.

The tools and versions used are defined in the
[Technology Stack Document](Technology%20Stack%20Document.md); when the tests run
in the delivery flow is defined in the
[Development Workflow Document](Development%20Workflow%20Document.md).

## 2. Testing philosophy

1. **Behavior-driven.** Tests describe *behavior* (what the command/query does
   for given input and state), not implementation. A refactor that preserves
   behavior must not require test changes.
2. **Test at the right layer.** Command/Query handlers and pure domain logic are
   unit-tested against repository **traits**; persistence and filesystem behavior
   are integration-tested through the real surface; HTTP/FFI parity is asserted
   across both transports.
3. **Isolation in unit tests.** Repository and auth-service collaborators are
   replaced by hand-written fakes (or `mockall` mocks where stateful expectations
   help); no real database, no real filesystem, no real network in unit tests.
4. **Realism in integration tests.** SQLite (on-disk or in-memory), a real temp
   directory for the filesystem, and the real HTTP server are exercised
   end-to-end through the REST contract.
5. **Parity is a first-class category.** Because HTTP and FFI must return
   identical results (NFR-09), every use case that exposes an operation over both
   surfaces gets a parity assertion that runs both and compares.
6. **Same pattern every time.** The per-use-case workflow in §8 is applied
   identically to every use case.

## 3. What to test for each use case

| Artifact produced | Test kind | Test location |
| --- | --- | --- |
| Command/Query handler (domain logic) | Unit | `alexandria-core` crate `tests/` mirrors |
| Repository implementation (SQLite) | Integration | `alexandria-core/tests/` (sqlite feature) |
| HTTP endpoint | Integration / functional | `alexandria-http/tests/` |
| FFI function | Integration | `alexandria-ffi/tests/` |
| HTTP ↔ FFI parity for an operation | Parity (contract) | `alexandria-ffi/tests/parity.rs` |
| Filesystem interactions (indexing, rename, text content) | Integration | integration tests using a temp dir |
| Auth service (local + external) | Unit + integration | `alexandria-core/tests/auth.rs` |

**Deliberately untested:** plain data-holder structs with no behavior, generated
OpenAPI scaffolding, and the cbindgen-produced C header (validated at build time
by the Flutter side). Serializers are exercised only through handler tests, not
in isolation.

## 4. Test project layout

The test tree mirrors the source tree of each crate. Integration tests live in
each crate's top-level `tests/` directory.

```
alexandria-core/
  src/
    catalog/
      commands/
      queries/
      repos/        # repository traits + sqlx impl
    auth/
    text/
  tests/
    catalog/        # mirrors src/catalog
    auth.rs
    text.rs
    common/mod.rs   # shared fixtures: temp db, temp fs, factories
alexandria-http/
  tests/
    catalog_api.rs
    auth_api.rs
    common/mod.rs
alexandria-ffi/
  tests/
    parity.rs       # HTTP vs FFI parity
    smoke.rs
```

## 5. Naming & structure

Every test is named with the **GivenWhenThen** pattern, snake_cased:

```
given_active_watchlist_when_add_video_then_progress_pending
given_deleted_file_when_restore_after_retention_then_not_found
given_local_mode_when_external_jwt_presented_then_unauthorized
```

Every test body follows the **Arrange / Act / Assert** shape:

```rust
#[test]
fn given_active_watchlist_when_add_video_then_progress_pending() {
    // Arrange — a watchlist exists; a fake repo returns it; the video is a VideoFile.
    let repo = FakeWatchlistRepo::with_watchlist(WATCHLIST_UUID);
    let handler = AddVideoHandler::new(repo);

    // Act
    let result = handler.add(WATCHLIST_UUID, VIDEO_UUID);

    // Assert
    assert!(result.is_ok());
    let progress = repo.progress_for(WATCHLIST_UUID, VIDEO_UUID).unwrap();
    assert_eq!(progress.state, WatchState::Pending);
}
```

## 6. Unit testing standard

### 6.1 Scope of a unit test

One test exercises exactly one Command/Query handler against trait fakes. A unit
test must not touch the database, the filesystem, or the network. It asserts the
handler's decision (state transition, validation outcome, error) and the
commands it issues to collaborators.

### 6.2 Test doubles

| Collaborator | Double |
| --- | --- |
| Repository trait (catalog, collections, bookmarks, watchlists, reading lists) | hand-written in-memory fake implementing the trait |
| Filesystem port | fake filesystem returning canned bytes / recording writes |
| Auth service | fake returning canned principal or an unauthorized error; `mockall` for stateful expectations |
| Time | a `Clock` trait with a fixed faked clock (for retention-window tests) |

Do not introduce a second mocking library; use `mockall` (see the
[Technology Stack Document](Technology%20Stack%20Document.md)) or hand-written
fakes only.

### 6.3 Coverage per handler

For each handler, walk this checklist when writing its tests:

- [ ] Happy path — the main flow succeeds and produces the expected state/result.
- [ ] Each validation failure (invalid input) — one test per invalid field/shape.
- [ ] Not-found — the referenced entity does not exist.
- [ ] Unauthorized — the caller is not authenticated (or the inactive auth mode was used).
- [ ] Invalid state / transition (where the entity has a lifecycle: deleted state, watch/read state transitions).
- [ ] Authorization — the owner is authorized; an unauthenticated caller is rejected.
- [ ] Boundaries — retention window boundary (just before / just after), per-episode/per-issue increments.

## 7. Integration & parity testing standard

### 7.1 Scope

One integration test exercises a use case through a real entry point (HTTP
endpoint or FFI function) backed by a real SQLite database and, where relevant, a
real temp filesystem. It asserts both the response and the resulting persisted
state.

### 7.2 External dependencies

| Dependency | In tests |
| --- | --- |
| SQLite | in-memory (or on-disk temp) SQLite via sqlx; migrations run at setup. |
| Filesystem | a per-test temp directory (the `tempfile` crate) holding sample files. |
| External auth service | a fake auth-service implementation with a fixed HS256 test secret; no network. |
| Local-login credentials | seeded into the test DB with an Argon2 hash of a known password. |

### 7.3 HTTP / FFI parity

For every use case exposed over both surfaces, a parity test in
`alexandria-ffi/tests/parity.rs` drove both the HTTP client and the FFI entry with
the same inputs and asserts byte-equivalent JSON results (modulo serialization
ordering). Parity failures point at exactly one place: a divergence between the
HTTP and FFI thin layers over the shared core handler.

### 7.4 Coverage per entry point

For each endpoint / FFI function, assert:

- [ ] Main flow — the response and the resulting persisted state.
- [ ] Each `AF-xx` alternative flow from the use case (not-found, unauthorized,
      invalid-input, invalid-transition, disk/IO failure, auth-mode mismatch,
      content-hash integrity).

## 8. Per-use-case workflow

Apply this every time a use case is implemented:

1. Read the use case and its traced `FR-xx` requirements from the
   [Use Case Specification Document](Use%20Case%20Specification%20Document.md).
2. Write unit tests for each Command/Query handler covering §6.3.
3. Write integration tests for the HTTP endpoint and the FFI function covering §7.4.
4. Add or extend a parity test asserting the two surfaces agree.
5. Run the suite; fix failures; re-run until green.
6. Commit the implementation and its tests together on the feature branch.

## 9. Performance requirements (NFR-01, NFR-02)

Functional requirements are verified by assertions; performance requirements
are **measurements**, and the two do not belong in the same gate. A throughput
floor is a statement about a machine, so asserting 500 files/sec inside
`cargo test --workspace` would make the suite report on the runner rather than
on the code.

`alexandria-core/tests/throughput.rs` measures both halves of NFR-02 — the
indexing rate, and that reads keep being served while a run is in flight
(NFR-01's p95 during load). Its tests are `#[ignore]`d and run on request:

```bash
cargo test --release -p alexandria-core --test throughput -- --ignored --nocapture
```

`--nocapture` is required: the measured figures are printed, and the figures
are the deliverable. `--release` matters just as much — NFR-02 scopes its
target to a shipped build, and an unoptimized one measures the debug profile
rather than the product. Two assertion modes:

| Mode | Asserts | For |
| --- | --- | --- |
| default | loose floors (≥ 50 files/sec, p95 < 2 s) | catching a real regression — a re-serialized walk, or blocking I/O back on the async runtime — without flaking on a shared runner |
| `ALEXANDRIA_NFR_STRICT=1` | the requirement itself (≥ 500 files/sec, p95 < 200 ms) | verifying NFR-02 on "a personal machine", which is what the requirement scopes it to |

Fixture size is tunable via `ALEXANDRIA_BENCH_FILES`,
`ALEXANDRIA_BENCH_FILE_BYTES`, and `ALEXANDRIA_BENCH_CONCURRENCY`.

**What the NFR-02 number covers.** Its fixture is plain text files, so it
measures the walk → classify → hash → persist pipeline (FR-FC-01..09) and
nothing else. Extraction is excluded there deliberately: folding ffmpeg's probe
speed into a figure labelled "Alexandria's indexing rate" would make the number
say less, not more.

**Extraction cost (FR-FC-25)** is measured separately, by the third test in the
same file. It generates a real fixture per metadata-carrying subtype — an
ID3-tagged WAV, an EXIF JPEG, a PDF, a CBZ, an MP4 encoded by ffmpeg itself —
runs each through the same indexer, and prints files/sec, ms/file, and each
format's rate as a percentage of the text baseline.

Those rows are **floors, not forecasts.** Each fixture is the smallest valid
file of its format, so a row isolates the fixed per-file cost — open the
container, find the metadata, parse it — with almost no payload to scale over.
Real media is orders of magnitude larger, and two costs grow with size:
hashing reads every byte, and ffmpeg may seek a long way to find its best video
stream. Read a row as "extraction costs at least this much per file, before
file size enters into it".

The test discards one warm-up round before the row it reports as the baseline.
Every row builds its own database and fixture tree, but the first row also
absorbs what a process pays exactly once — the first migration on a cold page
cache, the first touch of the sqlite and ffmpeg code paths, and on Windows the
anti-malware scan of a newly written binary. Those landed on `text`, the row
every other row is divided by, and the `vs text` column consequently reported
audio and document as *faster* than extracting nothing at all. That is
impossible: extraction is strictly extra work on the same pipeline. If that
column ever shows a figure above 100% again, suspect the harness before the
extractor.

CI runs these on pushes to `main` as a separate job carrying
`continue-on-error: true`, so the numbers are recorded over time and the
harness cannot rot, without a noisy runner turning a red build into something
to ignore. That job deliberately runs the **debug** profile and without
`ALEXANDRIA_NFR_STRICT` — it is checking that the harness still compiles and
still finds every fixture, not measuring the requirement, and paying for an
LTO release build on every push to `main` would buy a number the loose floors
do not read anyway. The `--release` invocation above is for the on-request run
that produces a figure worth recording.

That flag is load-bearing, not decorative. The loose floors resist runner
noise, but the harness also asserts **exact counts** — every fixture file
cataloged — and those are not noise-proof. A runner roughly 3.5× slower than
usual once pushed the concurrent writers past SQLite's `busy_timeout` and cost
the run two files out of two thousand, reddening `main` on a commit that was
fine. The indexer now retries a busy write, which removes that particular
cause; the flag is what stops the next unforeseen one from failing a build over
a measurement.

Read a red mark here as a prompt to compare the figures against the previous
run, not as a broken build.

### 9.1 Recorded measurements

Requirements that are measured rather than asserted are only verified when
someone writes the measurement down. This is that record — not a target to
regress against, since the next machine will read differently.

| | |
| --- | --- |
| Date | 2026-08-11 |
| Commit | `2929529` |
| Machine | Intel Core i9-13900K (24 cores / 32 threads), 32 GB RAM, NVMe SSD |
| OS | Windows 11 Pro 10.0.26200 |
| Toolchain | rustc 1.97.1, `--release` (`lto = true`, `codegen-units = 1`) |
| Command | `ALEXANDRIA_NFR_STRICT=1 cargo test --release -p alexandria-core --test throughput -- --ignored --nocapture` |

**NFR-02 — indexing throughput.** 2000 files of 4096 B, concurrency 4,
elapsed 2.33 s: **859 files/sec** against a target of ≥ 500.

**NFR-01 — read latency during indexing.** 259 reads issued while the run was
in flight: **p95 12.0 ms**, worst 29.4 ms, against a target of < 200 ms.

**FR-FC-25 — per-format extraction cost**, concurrency 4:

| format | files | files/sec | ms/file | vs text |
| --- | ---: | ---: | ---: | ---: |
| text (baseline) | 150 | 722 | 1.38 | 100% |
| audio (wav/id3) | 150 | 339 | 2.95 | 47% |
| image (jpeg/exif) | 150 | 315 | 3.18 | 44% |
| document (pdf) | 150 | 267 | 3.74 | 37% |
| comic (cbz) | 150 | 336 | 2.98 | 46% |
| video (mp4) | 37 | 323 | 3.10 | 45% |

Extraction roughly halves the per-file rate across every format — about
1.6–2.4 ms of fixed cost on top of a 1.4 ms baseline — with no format standing
out as pathological. Both NFR targets are met with 1.7× headroom on throughput
and 16× on latency, on hardware well above what the requirement means by "a
personal machine"; a modest laptop should be expected to sit closer to the
floor.

## 10. Running the suites

```bash
cargo test
```

| Suite | Command |
| --- | --- |
| All suites | `cargo test` |
| Unit tests only | `cargo test --lib` |
| Integration tests | `cargo test --test '*'` |
| Parity tests | `cargo test --package alexandria-ffi --test parity` |
| Auth tests | `cargo test --package alexandria-core --test auth` |
| Performance (§9) | `cargo test -p alexandria-core --test throughput -- --ignored --nocapture` |
| With coverage | `cargo tarpaulin` (optional) |

Categories are separated by crate and test file; parity tests live in their own
test target so they can be run in isolation, and auth tests are grouped in
`alexandria-core/tests/auth.rs`.