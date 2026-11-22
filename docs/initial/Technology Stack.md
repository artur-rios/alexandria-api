# Technology Stack — Alexandria

## Platform & Language

Rust. No version is pinned yet; the exact toolchain and crate versions will be
recorded as "latest stable at implementation time" in the formal
[Technology Stack Document](Technology%20Stack%20Document.md) after selection.

## Application Type

A layered back-end:

1. **Rust core library** — the domain logic (commands, queries, indexing,
   metadata, file references) in a reusable library crate.
2. **FFI surface** — the core library's C ABI, consumed directly by the Flutter
   desktop front-end in process.

The HTTP/REST JSON server that once sat beside them is removed: the core only
runs locally, for the person at the machine.

Recommended patterns: SOLID, Command/Query (CQRS-style) as the baseline for the
core library's operations.

## Data Storage

SQLite as the embedded, primary database for catalog metadata, folder
structure, bookmarks, watchlists, and deletion state. The design is open to
additional databases later if a need arises; SQLite is the starting point.

The API stores **only metadata and a path**, never file bytes. Indexing reads no
bytes to identify a file: size and modification time are the change signal
(FR-FC-09, FR-FC-10). Markdown and text file content is edited in place on
disk, and that write is the one thing that records a content hash (FR-TX-03).

## Data Access

Undecided. The selection of an ORM, query builder, or raw driver layer will be
decided in Phase 2 and recorded in the
[Technology Stack Document](Technology%20Stack%20Document.md).

## Authentication

**Removed (the core only runs locally for the person at the machine)**

There is no login. The core runs in process for the person at the machine, and
the catalog has the protection the operating system gives that person's own
folder.

## Testing

Rust's built-in `#[test]` framework with unit and integration tests in the
standard `cargo` layout. The full suite is run with:

```bash
cargo test
```

## External Dependencies

- **The local filesystem** — indexing reads file metadata and content hashes;
  Markdown/text edits write back to the source path.

No email, payments, identity providers, message brokers, or object storage are
involved.

## Deployment

Bundled with the Flutter desktop application, running on the user's own
machine. The desktop client links the FFI core and calls it in process.
Concrete packaging specifics are recorded as undecided until the
desktop integration is built.
