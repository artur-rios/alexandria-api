# The types an index run covers

**Date:** 2026-08-27
**Status:** approved
**Issue:** #122

## Problem

An index run records every supported file under its root. A music folder's
cover art is therefore catalogued as an image library, because `cover.jpg` is
an image and the classifier is right about that. What is missing is anywhere
for the owner to say what the folder is *for*.

## Design

### 1. A scope is a set of types, and absent means all

`IndexRequest` gains a scope: the file types the run records. Absent is every
supported type, so every existing caller keeps its behaviour and the parameter
is genuinely optional rather than nominally so.

The scope reaches the walk the same way the root does — as an argument to
`execute`, beside the run id. Nothing is persisted about it. A run's scope
matters only while it walks, and a column recording it would exist to answer a
question nobody has yet asked.

### 2. Where it applies

At the one place the walk already decides a file is not for it:

```rust
let Some(file_type) = classify_by_extension(&entry.name) else {
    return EntryOutcome::Skipped;
};
```

An out-of-scope file takes the same `Skipped` outcome as an unrecognised one.
That is the accurate tally: in both cases the run saw a file and chose not to
record it, and inventing a second counter would split one fact across two
numbers that every reader would then have to add together.

The check happens after classification rather than before, because the type is
what is being filtered on and only the classifier knows it.

### 3. An unspellable scope is refused, unlike an unspellable priority

`parse_priority` treats an unrecognised value as `Normal`, and the reason it
gives is that a client which cannot spell the value should get the safe
default rather than a rejected call.

There is no safe default here. A scope's fallback would be "every type" —
which is the *opposite* of what a caller asking for a narrower scope wants,
and it fails in the direction of cataloguing exactly the files the owner
excluded. So an unrecognised type name is `INVALID_INPUT`, and the caller
learns it rather than discovering it later in a library full of cover art.

Absent and empty stay all types. At the FFI boundary a null pointer and an
empty string are the same absence, and reading one as "index nothing" would
turn a caller's missing argument into a run that does nothing at all.

### 4. Refresh is untouched

A refresh discovers through the catalog, not through a walk (FR-FC-28), so it
re-checks what is already recorded and cannot pull in a type that was never
indexed. Giving it a scope would be a parameter with nothing to filter.

## Components

| Component | Change |
| --- | --- |
| `catalog/index_scope.rs` (new) | The set, its parse from wire names, and `includes`. |
| `catalog/commands/index.rs` | `IndexRequest` carries it; `execute` takes it; the walk skips what is out of it. |
| `catalog/file_type.rs` | Parse from the wire name `as_str` already writes. |
| `alexandria-ffi/src/lib.rs` | `alexandria_index_start` takes `types`, a comma-separated list. |
| `alexandria-http/src/routes/index.rs` | The start body takes `types`, an array. |

## Requirements impact

- **FR-FC-02** covers what a run records and gains the scope.
- **FR-FC-24** (HTTP↔FFI parity) already governs both transports; the new
  parameter is subject to it like every other.

## Testing

- A run scoped to audio over a folder of FLACs and JPEGs records the FLACs and
  skips the JPEGs — the owner's actual symptom.
- The skipped JPEGs are counted as skipped, not as failed.
- An absent scope records both, as today.
- An empty scope records both.
- A scope of two types records both and excludes a third.
- An unrecognised type name is refused as invalid input, and the run does not
  start.
- Both transports parse the same list into the same scope, with a payload that
  would differ if either side dropped it (FR-FC-24).

## Risks

The FFI signature changes, so alexandria-ui must pass the new argument. That
is a compile error at the binding rather than a silent misread, because the
argument is added to the end of a `#[no_mangle]` function the application
declares by hand.
