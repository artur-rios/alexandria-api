# Review — 2026-08-23

A full review of `alexandria-api`, `alexandria-ui`, and `alexandria-docs`:
implementation against documentation, bugs, inconsistent behavior, and
improvements. This file records what was found in **this** repository and what
was done about it. The other two repositories carry their own copy covering
theirs.

## Baseline

Everything below is drift that the existing checks do not catch. All four were
green before any change was made, and again after:

| Check | Result |
| --- | --- |
| `cargo test --workspace` | 1201 passed, 52 suites, 0 failed |
| `cargo clippy --workspace --all-targets` | clean |
| `flutter test` (alexandria-ui) | 1649 passed |
| `flutter analyze --fatal-infos --fatal-warnings` (alexandria-ui) | no issues |

That is worth stating plainly: **no finding here was a failing test.** They are
all cases where the documentation described a system that no longer exists, or
where an identifier pointed at the wrong requirement.

---

## Findings

### A-01 — Documentation still described hash-based indexing · **fixed**

`FR-FC-09` and `FR-FC-10` replaced full-file hashing with the size/mtime stat
pair. The System Requirements, Operations & Infrastructure, and Technology Stack
documents were updated with that change; the informal set and the README were
not.

| Location | Claimed |
| --- | --- |
| `README.md` — F-01 heading | "classify them by type, **hash their bytes**" |
| `README.md` — banner | "Metadata + path/**content-hash** only" |
| `docs/initial/Project Overview.md` | "stores metadata plus a path/**content-hash** reference" |
| `docs/initial/Technology Stack.md` | "only metadata and a **path/content-hash** reference" |
| `config.toml.example` — `[indexing]` | "**Hashing** runs on a blocking thread pool" |

This is the most consequential documentation defect in the review, because the
cost model follows from it: a reader who believes indexing hashes will
mis-predict scan time by orders of magnitude on a large library, and will reason
incorrectly about what a re-index does.

**Fixed** — every location now describes the stat pair, and says where a content
hash does still come from (`FR-TX-03`, and nowhere else).

### A-02 — `config.toml.example` misdescribed the thumbnail cache key · **fixed**

Claimed entries were "keyed by the file's content hash, so a re-index that
changes a file's bytes invalidates its thumbnail automatically."

The implementation keys on **uuid and mtime** — deliberately, and with the reason
in a comment at `playback/thumbnail.rs:85`: hashing a multi-gigabyte video to
decide whether its thumbnail is stale would cost more than making the thumbnail.

Documentation-only, but it described an invalidation mechanism that would be
broken if it were true, since content hashes are no longer computed at all.

**Fixed** — the comment now describes the real key and why it is that key.

### A-03 — `logging.level` breaks the documented environment-variable rule · **fixed**

`config.toml.example` stated the override pattern is `ALEXANDRIA_<SECTION>_<KEY>`
with `auth.local_db` as the only exception. `logging.level` is in fact read from
`ALEXANDRIA_LOG_LEVEL`, not `ALEXANDRIA_LOGGING_LEVEL`.

**Fixed** as documentation rather than as code. Renaming the variable would break
every existing deployment that sets it, to buy consistency in a rule that can
simply name its two exceptions instead. Both are now named.

### A-04 — Run-control operations cited the wrong requirement and use case · **fixed**

Pause, resume, cancel, and the outstanding-runs listing were all annotated
`FR-FC-28` — which is only the *status query* — and all attributed to `UC-42`,
which is likewise only the query. The specification assigns them separately:

| Operation | Was | Correct |
| --- | --- | --- |
| Query one run | UC-42 / FR-FC-28 | unchanged, was right |
| Pause | UC-42 / FR-FC-28 | **UC-48 / FR-FC-32** |
| Resume | UC-42 / FR-FC-28 | **UC-48 / FR-FC-33** |
| Cancel | UC-42 / FR-FC-28 | **UC-48 / FR-FC-34** |
| List outstanding runs | UC-42 / FR-FC-28 | UC-42 / **FR-FC-35** |

The specification itself was already correct — `UC-48` exists and cites
`FR-FC-32` … `FR-FC-34` properly. The drift was entirely in source annotations.

It mattered beyond tidiness: these comments are the doc comments `cbindgen`
copies into the generated C header, which `alexandria-ui` vendors and runs
through `ffigen`. The wrong identifiers had already propagated into the
front-end's generated bindings and were then copied by hand into roughly eight
more of its files.

**Fixed** in `alexandria-ffi/src/lib.rs`, `alexandria-http/src/routes/runs.rs`,
`catalog/commands/run_control.rs`, and `catalog/queries/active_runs.rs`. The
front-end's copies are corrected in its own branch.

### A-05 — README reported two delivered use cases as outstanding · **fixed**

`UC-41` (register the local account, `#96`) and `UC-42` (query a run, `#99`) were
marked ☐. Both are fully routed, handled, and tested — `auth_register_api.rs`,
`run_status_api.rs`, `run_control_api.rs`, plus parity coverage — and the
README's own prose documents both as available.

`UC-48` had no row at all.

**Fixed** — both flipped to ☑, `UC-48` added, and the counts corrected: F-01
2/3 → 3/3, F-09 3/4 → 4/4, total **43/45 → 45/45**.

### A-06 — No document described the running system · **fixed**

The repository specified what the system *shall* do in seven requirements
documents, and described what it *is* in a README aimed at getting it built and
running. Nothing described what it actually *does* — the order of operations, the
state each run moves through, which failures are absorbed rather than reported,
and why.

That gap is where every finding above came from: with no document that has to
stay true to the implementation, drift has nowhere to be noticed.

**Added** [`docs/System Behavior Document.md`](System%20Behavior%20Document.md)
— startup and reconciliation, the request path and error model, the indexing
subsystem in depth (run states, the start/execute split, the library-root bound,
concurrency and priority, progress publication, pause/resume/cancel and the races
between them, re-index, metadata extraction), playback and byte streaming, the
deletion lifecycle, and the three authentication modes. Fourteen diagrams.

It closes with §9, a table of behaviors that are easy to assume the other way
round — the list a reader is most likely to get wrong, stated explicitly.

---

## Reviewed and found correct

Recorded so a later reader knows these were checked rather than skipped.

- **The indexing and run-control implementation.** The race handling between a
  walk's terminal write and a concurrent control call is genuinely subtle — the
  `segment` counter, the conditional writes, and `RunCell::raise`'s
  no-downgrade guard — and it is correct, tested, and commented with the
  reasoning intact.
- **`FR-FC-26`, the library-root bound.** Canonicalizes both sides and compares
  by path component, so traversal segments, trailing separators, and symlinks
  cannot escape it. Fails closed on an unresolvable configured root, with a
  distinct message, rather than silently degrading to unconstrained indexing.
- **Re-index semantics.** Matches `FR-FC-10` and `FR-FC-11` exactly, including
  the case a reader would miss: a file that returned to disk while marked
  missing is refreshed even when its stat pair is unchanged, because
  `missing_at` has to be cleared.
- **The playback surface.** Resolves and guards before any byte is written;
  converts the file server's bare `404` into the API's own error envelope,
  closing the window between the stat and the open; stamps playback headers only
  on the statuses that actually carry bytes.
- **MIME and classification tables.** Mirror each other exactly, so every
  extension the indexer admits has a MIME answer.
- **Both surfaces' priority parsers.** Agree byte for byte, including the
  deliberate difference between `start` (unreadable → `normal`) and `resume`
  (unreadable → keep the current width).
- **The authorization gate's ordering.** Runs before route extractors, so an
  unauthenticated caller learns nothing about whether its payload parsed.

---

## Not done, and why

- **`FR-FC-02` … `FR-FC-07`, `FR-FC-31`, `FR-FC-32`, `FR-FC-34`, `FR-FC-35`,
  `FR-AU-12`, and several `NFR`/`IR` identifiers are never cited in source.**
  Annotation coverage is not a correctness property, and adding citations
  mechanically would be churn without a reader. Listed here so the gap is known.
- **The `logging.level` environment variable was not renamed.** See A-03.
