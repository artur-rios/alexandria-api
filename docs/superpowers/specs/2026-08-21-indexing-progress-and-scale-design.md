# Design: Indexing progress, run control, and scale

**Date:** 2026-08-21
**Status:** Approved, ready for implementation planning
**Tracks:** Extends an existing capability area — File Catalog (FC). Amends
FR-FC-09, FR-FC-10, FR-FC-27, FR-FC-29 and NFR-02; adds run control and
progress reporting.

## Context

Indexing a 418 GB library of 12,264 FLAC files takes tens of minutes, reports
nothing at all while it runs, and cannot be stopped and picked up later. All
three failures come from the same place.

`IndexHandler::index_entry` hashes every byte of every file with SHA-256
before persisting it. The handler's own documentation says so plainly: "the
per-file work is dominated by hashing the bytes." A 418 GB library therefore
costs 418 GB of disk reads, and the cost scales with the library's *size*
rather than its file *count* — the opposite of what NFR-02's "500 files per
second" implies. The 12,264 figure is irrelevant to how long that scan takes;
the bytes behind it are the whole of it.

Progress does not exist. `IndexHandler::execute` tallies outcomes into a fold
and writes them once, through `runs.finish`, when the walk is over. A run in
flight has status `running` and nothing else — `CatalogRun::counts` is `None`
until it terminates, which the FFI gateway on the front end already comments
on. There is no number for a client to draw a bar from because the core never
publishes one.

Run control does not exist either. The FFI surface is `alexandria_index_start`,
`alexandria_index_refresh_start`, `alexandria_index_count_files` and
`alexandria_index_run_status_json`. A run, once started, runs to completion or
dies with the process, and FR-FC-29's startup reconciliation records the latter
as `interrupted` — a terminal state that names the loss without offering a way
back from it.

This design fixes the cost first, because the other two get much easier once it
is fixed. A scan that no longer reads whole files can be paused within
milliseconds instead of waiting on a multi-gigabyte hash, and it can be resumed
by re-walking rather than by checkpointing a cursor, because re-walking is
suddenly cheap.

This is the core half of the work. The front end that consumes it — the
progress bar, the background activity strip, the resume prompt — is specified
separately in `alexandria-ui`.

## Decisions

1. **Change detection moves from content hashing to size and mtime.**
   `files` gains `size_bytes` and `mtime`, both captured from the directory
   entry during the walk, where they are free. Indexing records them and reads
   no file bytes at all. Refresh compares them and stops there: unchanged size
   and mtime is an unchanged file, and a difference in either is a changed one.
   Refresh never hashes. When it records a change it writes the new size and
   mtime and sets `content_hash` back to `NULL`, so the stale hash cannot
   outlive the bytes it described and the next caller that needs one computes
   it fresh.

   The alternative of keeping the hash and merely adding a stat fast path was
   considered and rejected. It makes re-index fast while leaving the *first*
   index — the slow one, the one the owner waits on — exactly as slow as it is
   now. A sampled hash over head, tail and size was also considered; it keeps a
   content-derived key at the price of a genuine, if unlikely, false
   "unchanged" for two files differing only in the middle. Size and mtime have
   the same class of blind spot with none of the reads.

   The blind spot this accepts: a file edited in place to exactly the same byte
   length with its mtime preserved reads as unchanged. Producing that takes
   deliberate effort, and a manual re-index is the escape hatch.

2. **`content_hash` becomes nullable and is computed on demand.**
   `NULL` means "not computed". A repository helper, `ensure_content_hash`,
   computes and stores it the first time something genuinely needs it. After
   decision 3, the only caller is UC-33's optimistic-concurrency check on a
   text edit, which operates on one small file while the owner is already
   waiting on that specific file. Refresh is deliberately not a caller — see
   decision 1.

   The column is made nullable by amending the baseline migration rather than
   by stacking a new one. `sqlx::migrate!` checksums migration files, so this
   invalidates every existing database: they are deleted and re-created rather
   than migrated. The project is pre-release, and a full re-index after this
   change is cheap by construction — that is the point of the change.

3. **Thumbnails are re-keyed off uuid and mtime.**
   `thumbnail.rs` keys its cache on `format!("{}-{}", file.content_hash,
   THUMBNAIL_MAX_DIM)`. Left alone against a lazy hash, the first thumbnail of
   every video would force a full-file SHA-256 — moving the 418 GB out of
   indexing and into browsing, one unpredictable multi-gigabyte stall at a
   time. That is worse than the problem being solved, because at least the scan
   was something the owner asked for.

   The key becomes `uuid-mtime-maxdim`. The uuid is already unique and stable,
   and folding in mtime preserves the invalidation-on-change the content hash
   was providing.

4. **`skipped` splits into `skipped` and `alreadyCataloged`.**
   `index_entry` currently returns `Skipped` both for an unsupported extension
   and for a path already in the catalog. Resume works by re-walking and
   skipping what is done, so a run paused at 8,000 of 12,264 and resumed would
   report roughly 8,000 spurious `skipped` — a tally that lies about what
   happened. Splitting the counter makes resumed runs honest, and is more
   informative even for runs that are never paused.

5. **Progress is an in-memory counter, flushed periodically.**
   A `RunRegistry` holds one cell per in-flight run: `processed` and `total` as
   atomics, plus the phase and the control signal. Each finished entry bumps a
   counter — no lock, no database write on the hot path. A flusher writes the
   cell into the run's row every two seconds and on every state change.

   `run_status` reads the row and overlays the live cell when one exists. A
   query during a run is therefore exact rather than up to two seconds stale,
   and a query after a restart falls back to the last flush — which is what
   lets a paused run still report "8,412 of 12,264" at the next launch.

6. **A run has two phases, and only one of them has a percentage.**
   `StdFilesystem::collect` materializes the whole entry list before any file
   is processed. That is convenient — it is how `total` becomes known — and at
   this scale it costs seconds and a few megabytes. The stretch is reported as
   phase `discovering`, with no total and no percentage; the rest is phase
   `processing`, where `total` is fixed and never moves.

   Materializing the walk does not scale to a million-file library. That is
   accepted here and left for a later design; streaming the walk would trade
   the total away, and the total is what this design exists to provide.

7. **The core reports counts and elapsed active time; the front end computes
   ETA.**
   The run body carries `processed`, `total`, `startedAt`, and `activeMillis`
   — wall time actually spent processing, with paused stretches subtracted. A
   remaining-time estimate needs smoothing tuned to how often the client polls,
   which makes it a presentation concern. `activeMillis` is the one input a
   client cannot derive for itself, so the core provides that and stops there.

8. **Pause, cancel, and resume share one mechanism.**
   The registry cell's `signal` is read as `None`, `Pause`, or `Cancel`. Each
   per-entry future checks it before doing work and short-circuits when it is
   set; the `buffer_unordered` window drains and the run records itself.
   Because per-file work is now a stat and a header read, that window empties
   in milliseconds. Decision 1 is what makes pause feel immediate.

   During `discovering` the signal is honoured at the phase boundary rather
   than inside the walk: `StdFilesystem::collect` is one blocking call with no
   interruption point. Discovery is seconds, so a pause requested there lands
   when the walk ends and before any file is touched.

   Pause writes `paused` and `paused_at`. Cancel writes `cancelled` and is
   terminal — it is the "I started this on the wrong folder" case, distinct
   from pause's "I will come back to this".

9. **Resume re-walks; it does not checkpoint.**
   Resume is valid only from `paused`. It adds `now - paused_at` to
   `paused_millis`, clears `paused_at`, sets `running`, and spawns `execute`
   again on the same run id with the segment counters zeroed.

   Rediscovering `total` and counting `processed` from zero is deliberate. The
   first several thousand entries are a single indexed-path lookup each and fly
   past in seconds; then the run slows to real work. There is no cursor to keep
   honest and no drift to correct, and the arithmetic stays true.

   The consequence is that a resumed run's tally describes its last segment: a
   run paused at 8,000 and resumed finishes reporting `scanned 12,264, indexed
   4,264, alreadyCataloged 8,000`. A client reads "in the library" as
   `indexed + alreadyCataloged`, which lands on the right number.

10. **Startup reconciliation pauses rather than interrupts.**
    `interrupt_running` becomes `pause_running`: a run found `running` at
    launch becomes `paused`, and `interrupted` leaves the status enum entirely.
    Closing the application mid-scan therefore leaves a run the owner is
    offered, not a loss they are informed of. Nothing starts by itself at
    launch — resuming is an explicit act.

    A refresh resumes as safely as an index. It iterates cataloged paths and is
    idempotent, and after decision 1 re-running it is cheap.

11. **Throttling is a priority chosen at run start, not a live slider.**
    `buffer_unordered(n)` fixes its width when the stream is built, so a live
    knob means replacing it with a semaphore whose permits grow and shrink —
    the most invasive change available here and the likeliest to introduce a
    subtle concurrency bug. Instead, starting or resuming a run takes
    `priority: Normal | Low`, mapping to `indexing.concurrency` (default 4) and
    a new `indexing.low_priority_concurrency` (default 1). The choice is stored
    on the run, so resume defaults to it. Changing your mind mid-run means
    pausing and resuming, which decision 9 made nearly free.

    A semantic priority rather than a raw thread count: the client should not
    have to invent a number.

12. **The core answers "what is outstanding?"**
    A new `alexandria_index_runs_active_json` returns every non-terminal run.
    Today the front end tracks run ids itself, one `lastRunId` per registered
    source in its own settings store. Both a global background-activity
    indicator and a resume-at-launch prompt need one question answered across
    all runs at once, and the core is the only place that can answer it
    honestly.

## Schema

`files` — amended in the baseline migration:

| Column | Change |
| --- | --- |
| `content_hash` | `TEXT NOT NULL` → `TEXT` (nullable; `NULL` = not computed) |
| `size_bytes` | new, `INTEGER` |
| `mtime` | new, `TEXT` (RFC 3339) |

`catalog_runs` — amended in the baseline migration:

| Column | Purpose |
| --- | --- |
| `phase` | `discovering` \| `processing`; `NULL` once terminal |
| `total` | entries discovered; `NULL` while discovering |
| `processed` | entries finished in the current segment |
| `paused_at` | when the current pause began |
| `paused_millis` | accumulated paused time across segments |
| `already_cataloged` | decision 4's new counter |
| `concurrency` | the priority the run was started or resumed with |

`status` gains `paused` and `cancelled`, and loses `interrupted`.

## State machine

```
running   → paused | complete | failed | cancelled
paused    → running | cancelled
complete  → (terminal)
failed    → (terminal)
cancelled → (terminal)
```

A control aimed at the wrong state returns a new `RUN_ERR_INVALID_STATE`
rather than silently doing nothing. An unknown id keeps returning
`RUN_ERR_NOT_FOUND`, and an unauthenticated caller `RUN_ERR_UNAUTHORIZED`, as
today.

## Surfaces

FFI (`crates/alexandria-ffi/src/lib.rs`, header regenerated):

| Call | Shape |
| --- | --- |
| `alexandria_index_pause` | `(run_id, token) -> c_int` |
| `alexandria_index_resume` | `(run_id, token) -> IndexStartResult` |
| `alexandria_index_cancel` | `(run_id, token) -> c_int` |
| `alexandria_index_runs_active_json` | `(token) -> RunJsonResult` |
| `alexandria_index_start` | gains `priority` (breaking) |
| `alexandria_index_refresh_start` | gains `priority` (breaking) |
| `alexandria_index_run_status_json` | body gains `phase`, `total`, `processed`, `activeMillis`, `pausedAt`, `alreadyCataloged` |

HTTP, at parity per FR-FC-24:

- `POST /v1/index/runs/{runId}/pause`
- `POST /v1/index/runs/{runId}/resume`
- `POST /v1/index/runs/{runId}/cancel`
- `GET /v1/index/runs?status=active`
- `priority` accepted on `POST /v1/index` and `POST /v1/index/refresh`

The two breaking FFI signatures are acceptable because both repositories move
together and `ffigen` regenerates the Dart bindings from the header.

## Components

| Unit | Responsibility | Depends on |
| --- | --- | --- |
| `RunRegistry` | Per-run process state: progress atomics, phase, control signal | nothing |
| Progress flusher | Writes registry cells into `catalog_runs` on an interval and on state change | `RunRegistry`, `CatalogRunRepository` |
| `IndexHandler` | Walk, classify, stat, persist, prefill tags; honours the signal | `RunRegistry`, `Filesystem`, `CatalogRepository` |
| `RefreshHandler` | Stat-compare every cataloged path; hash only on a difference | same |
| Run control handlers | Pause, resume, cancel; enforce the state machine | `RunRegistry`, `CatalogRunRepository`, `AuthService` |
| `run_status` query | Row, overlaid with the live cell when present | `RunRegistry`, `CatalogRunRepository` |

Each is testable against trait fakes, as the existing handlers already are.

## Error handling

Unchanged in shape: a failure against one file is counted in `failed`, logged
at `warn`, and the walk continues; only a failure to list the root at all fails
the run. Two additions:

- A control call against a run in the wrong state returns `RUN_ERR_INVALID_STATE`.
- A progress flush that fails is logged at `warn` and does not fail the run.
  The authoritative counter is the in-memory cell; a missed flush costs
  accuracy after a restart, not correctness.

## Documentation

This project is spec-driven, so the requirement documents are part of the
deliverable:

- New FRs for run control (pause, resume, cancel) and for progress reporting.
- FR-FC-09 and FR-FC-10 rewritten around stat-based change detection.
- FR-FC-27 extended with the two new statuses and the new counters.
- FR-FC-29 rewritten from interruption to pause.
- A new use case in the Use Case Specification for pausing and resuming a run.
- NFR-02 restated: the rate is now independent of the library's size, which is
  the substance of this change.

## Testing

Per the Testing Specification's layering:

- Core unit tests against fakes: the state machine's legal and illegal
  transitions, the split counters, stat-based change detection, and the
  lazy-hash helper.
- A resume test asserting the tally arithmetic of decision 9.
- `tests/throughput.rs` gains a large-file case. The existing fixtures are
  small text files, which is exactly why the current suite could not have
  caught this — a size-dominated cost is invisible to a size-free fixture.
- FFI parity tests for the four new calls and the two changed signatures.
- HTTP route tests for the four new endpoints.
