# Design: Query an index or refresh run (F-01/F-02 — UC-42)

**Date:** 2026-08-13
**Status:** Approved, ready for implementation planning
**Tracks:** Extends the existing capability areas — File Catalog (FC), UC-01 and
UC-02. Issue #99.

## Context

UC-01 (index) and UC-02 (re-index) are asynchronous by requirement: FR-FC-08
says indexing must not block reads. Both follow the same shape. `start()`
authenticates and returns a freshly minted `run_id`; the transport answers
`202 Accepted` with that id and spawns `execute()` on a background task;
`execute()` walks the catalog, tallies an outcome, logs it, and drops it.

The run id is therefore a receipt for something the caller can never ask about
again. `routes/refresh.rs` states the gap outright, in a comment above the
spawn: *"nothing else observes this task's result."*

What a client can observe instead are the catalog counts — how many files
exist, how many are marked missing. Neither means "the run is complete."
`RefreshHandler::refresh_one` processes cataloged paths concurrently, and a
single run does two independent kinds of work: re-hashing files whose content
changed, and marking vanished files missing. Those land in no fixed order, so a
client that treats a missing-count increment as "done" can read rows whose
hashes have not been rewritten yet. It sees a half-finished run and cannot tell.

This is not hypothetical. It broke `main` in CI run 31734270643: the UC-02
parity test waited on the missing count, then compared rows across the HTTP and
FFI surfaces, and the recorded hashes decoded to `sha256("audio-v1")` on one
surface and `sha256("audio-v2-CHANGED")` on the other. One leg had re-hashed
the changed file and the other had not. PR #98 fixed that test by waiting on the
condition its assertion actually depended on. It did not close the underlying
gap: a real client has no better signal available than the one the test was
misusing.

This design makes a run observable. It does not make it synchronous — FR-FC-08
is unchanged, and `202 Accepted` remains the answer to starting one.

## Decisions

1. **Persist the run, keyed by the id already returned.** Every piece needed
   already exists: `start()` mints a `Uuid`, the transport hands it to the
   caller, and `execute()` computes a complete `IndexOutcome` / `RefreshOutcome`
   with per-kind counts. The only thing missing is that nothing writes them
   down. A status query keyed by that id is the smallest change that closes the
   gap, and it needs no new identifier or handshake.

2. **A SQLite table, not an in-memory map.** A map behind a lock in `Services`
   needs no migration, but every run record vanishes on restart, and a client
   polling across one gets "unknown run" — indistinguishable from a bad id. A
   table survives restart, gives the run a history, and makes an interrupted run
   detectable rather than invisible.

3. **Cover index and refresh together.** The two commands have the identical
   start/spawn/discard shape. Building this for refresh alone means writing the
   same machinery a second time later, and a client is equally blind after an
   index as after a refresh.

4. **Reconcile interrupted runs at startup.** A row written as `running` whose
   process then dies would otherwise say `running` forever, and a client would
   poll a dead run indefinitely — a milder version of the ambiguity this design
   exists to remove. Runs live in-process and are never resumed, so a `running`
   row observed at startup provably has no task behind it. Marking those
   `interrupted` is sound, not a heuristic, and it avoids pushing a staleness
   policy onto every client.

5. **Per-kind count columns, not a generic blob.** `scanned` is meaningless for
   a refresh and `marked_missing` for an index. Collapsing them into shared
   `processed` / `changed` columns would discard distinctions `IndexOutcome` and
   `RefreshOutcome` already draw, and a JSON blob column would be flexible at
   the cost of being self-describing to neither a reader nor a query. The
   columns are nullable and populated per kind.

## UC-42 — Query an index or refresh run

| Field | Value |
| --- | --- |
| **ID** | UC-42 |
| **Name** | Query an index or refresh run |
| **Actors** | Owner |
| **Description** | Report the status and outcome of an index (UC-01) or re-index (UC-02) run, given the run id returned when it was started. |
| **Preconditions** | The caller is authenticated; a run was started and its id retained. |
| **Postconditions** | None — this is a query. The catalog is unchanged. |
| **Requirements** | FR-FC-24, FR-FC-27, FR-FC-28, FR-FC-29 |

**Main Flow**

1. The caller submits a run id.
2. The system confirms the caller is authenticated as the owner.
3. The system reads the run record for that id.
4. The system returns the run's kind, status, start time, finish time when it
   has one, and the outcome counts for its kind.

**Alternative Flows**

| ID | Condition | Outcome |
| --- | --- | --- |
| AF-01 | No run exists with that id | The system responds with a not-found error. |
| AF-02 | The caller is not authenticated | The system denies with an unauthorized error. |
| AF-03 | The run is still executing | The system returns it with status `running`; the count fields are absent, since no tally exists until the walk finishes. |
| AF-04 | The run could not proceed at all (the catalog was unreadable, or the root could not be walked) | The system returns it with status `failed` and the underlying error message. |
| AF-05 | The run was executing when the process stopped | The system returns it with status `interrupted` — no task is executing it and it will not resume. |

A run whose walk completed with per-file failures is `complete`, not `failed`:
those are counted in its `failed` tally and the walk deliberately continues past
them. `failed` is reserved for a run that could not proceed at all. The
distinction already exists inside `execute()` — one unreadable file must not
abandon the rest of the catalog — and this surfaces it.

That tolerance is currently implemented and documented only in the handlers'
doc comments; no functional requirement states it. FR-FC-27 below is written to
say it explicitly, so the status this design reports is anchored to a stated
requirement rather than to code commentary.

## New functional requirements

| ID | Requirement |
| --- | --- |
| FR-FC-27 | The system shall record every index and re-index run: its id, kind, start time, terminal status, finish time, and the outcome counts for its kind. A run whose walk completes shall be recorded `complete` even when individual files failed — those are counted in the run's `failed` tally, and one file's failure shall not abandon the rest of the walk. A run that could not proceed at all shall be recorded `failed` with the underlying error. |
| FR-FC-28 | The system shall expose a run's recorded status and outcome to an authenticated caller, given the run id returned when the run was started, over both the HTTP and FFI surfaces. |
| FR-FC-29 | The system shall, at startup, mark every run still recorded as running as interrupted; runs execute in-process and are never resumed. |

FR-FC-08 is unchanged: runs stay asynchronous, and starting one still answers
immediately. This makes a run observable, not synchronous.

## Data model

One migration adding one table.

```sql
CREATE TABLE IF NOT EXISTS catalog_runs (
    id              TEXT PRIMARY KEY,
    kind            TEXT    NOT NULL,
    status          TEXT    NOT NULL,
    root            TEXT,
    started_at      TEXT    NOT NULL,
    finished_at     TEXT,
    scanned         INTEGER,
    indexed         INTEGER,
    skipped         INTEGER,
    refreshed       INTEGER,
    marked_missing  INTEGER,
    unchanged       INTEGER,
    failed          INTEGER,
    error           TEXT
);

CREATE INDEX IF NOT EXISTS idx_catalog_runs_started_at ON catalog_runs (started_at);
```

`kind` is `index` or `refresh`. `status` is `running`, `complete`, `failed`, or
`interrupted`. `root` is the indexed root for an index run and `NULL` for a
refresh, which takes no root. `finished_at` and every count are `NULL` while the
run is `running`; `error` is set only when `status` is `failed`.

Run rows are kept indefinitely. A row is a few hundred bytes and runs are
started by the owner rather than on a timer, so pruning is speculative until
there is evidence it is needed.

## Domain

A new module `catalog/runs.rs` holds the run model, its repository port, and
the SQLite adapter together — the shape `auth/local.rs` already uses for
credentials and sessions. It does not go in `catalog/repos.rs`, which is
already 1,200 lines and would only grow less navigable.

The `CatalogRunRepository` port carries the run lifecycle:

- `start(id, kind, root, started_at)` — write the `running` row.
- `finish(id, outcome, finished_at)` — terminal row from a completed walk.
- `fail(id, error, finished_at)` — terminal row for a run that could not proceed.
- `get(id)` — the record, or `None` (AF-01).
- `interrupt_running(now)` — mark every lingering `running` row `interrupted`,
  returning how many were reconciled.

Both handlers take the run repository as a collaborator, alongside the auth
service, catalog repository, filesystem and clock they already hold.
`IndexHandler::start` and `RefreshHandler::start` write the `running` row before
returning the id they already mint. `execute()` writes the terminal row — the
`complete` row from the outcome it already computes, or the `failed` row on its
own error path before returning that error.

Writing the terminal row inside `execute()` rather than at the spawn sites is
deliberate. There are four spawn sites — HTTP and FFI, index and refresh — and
putting the lifecycle in the handler means the recording cannot be forgotten by
a fifth caller, and cannot drift between transports. It also keeps the whole
lifecycle in one layer: `start()` mints the id and opens the record, `execute()`
closes it. The transports keep their existing `tracing::error!` on the `Err`
branch; the comment reading "nothing else observes this task's result" is what
this design deletes, since the record now does.

Adding the collaborator changes both handlers' constructors, which ten call
sites across `services.rs` and the core tests must be updated for. That churn is
mechanical, and it buys the guarantee that a started run is always a recorded
run.

`interrupt_running` is called once from `build_services` at startup, before any
handler is constructed.

A `GetRunStatusHandler`, generic over the auth service and the run repository as
every other query handler is, authenticates and returns the record.

## Interfaces

### HTTP

`GET /v1/index/runs/{runId}`, inside the blanket `require_auth` gate.

Response `200`:

```json
{
  "runId": "…",
  "kind": "refresh",
  "status": "complete",
  "startedAt": "…",
  "finishedAt": "…",
  "refreshed": 1,
  "markedMissing": 1,
  "unchanged": 0,
  "failed": 0
}
```

An index run carries `root`, `scanned`, `indexed`, `skipped`, `failed` instead.
Fields that are `NULL` for the run's kind or status are omitted rather than sent
as `null`, so a `running` run carries no counts and a refresh carries no `root`.

Errors: `404` (AF-01), `401` (AF-02).

### FFI

`alexandria_index_run_status_json(run_id: *const c_char) -> RunJsonResult`,
following the existing FFI shape — the same JSON body the HTTP route returns,
`#[allow(unsafe_code)]` on `#[no_mangle]`, `services_slot()` / `cstr_lossy` /
`runtime().block_on`, and a `map_run_err` mapping `NotFound` to a
`RUN_ERR_NOT_FOUND` code.

`RunJsonResult` is a new struct mirroring the existing per-domain
`*JsonResult` types (`FileJsonResult`, `AuthJsonResult`, and the rest): a
`c_int` status plus an owned `*mut c_char` JSON body freed with
`alexandria_free_string`. The index surface has no JSON-result type to reuse —
`IndexStartResult` carries a run id, not a body, and `alexandria_index_files_json`
returns a bare `*mut c_char` with no status channel, which cannot express
AF-01's not-found.

This satisfies FR-FC-24 (dual-transport parity) for the new operation.

## Testing

**Core unit tests** against in-memory fakes: each lifecycle transition
(`running` → `complete`, `running` → `failed`), `get` returning `None` for an
unknown id, authorization rejection, and `interrupt_running` flipping only
`running` rows while leaving terminal ones untouched.

**A test that a run with per-file failures is `complete`, not `failed`** — the
distinction AF-04 draws is the one most likely to be implemented backwards.

**HTTP integration tests** over the real router and a real temp SQLite database:
`200` and body shape for a completed run of each kind, `404` for an unknown id,
`401` unauthenticated, and the one that motivates the whole design — start a
refresh, poll the run until `complete`, then assert the catalog rows are fully
settled. That is the assertion the UC-02 parity test could not previously make,
and it is why PR #98 had to reach into hash comparisons instead.

**An HTTP/FFI parity test** asserting both surfaces return the same body for the
same run.

## Out of scope

- **A "latest run" query.** A client receives the id in the `202`. A second
  lookup path is speculative until something needs it; a Flutter "last refreshed
  at" label is the plausible reason, and it can be added when that exists.
- **Cancelling a run.** Nothing asks for it, and it needs cooperative
  cancellation threaded through the concurrent walk.
- **Progress reporting during a run** (files done so far). The walk tallies only
  at the end; per-file progress means writing on every file, which would put the
  status table in contention with the catalog writes the run is already making.
  `running` versus terminal is what the gap actually requires.
- **Pruning run history.** See the data model note.
- **Making runs synchronous.** FR-FC-08 requires the opposite.
