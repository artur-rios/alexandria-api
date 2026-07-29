# Technology Stack — Alexandria

## Platform & Language

Rust. No version is pinned yet; the exact toolchain and crate versions will be
recorded as "latest stable at implementation time" in the formal
[Technology Stack Document](Technology%20Stack%20Document.md) after selection.

## Application Type

A layered back-end:

1. **Rust core library** — the domain logic (commands, queries, indexing,
   metadata, file references) in a reusable library crate.
2. **HTTP/REST JSON API** — a server crate that exposes the core library over
   an HTTP/JSON interface for any external caller in any language.
3. **FFI surface** — the same core library consumable directly by the Flutter
   desktop front-end via FFI, so the front-end can choose HTTP or FFI per
   operation without behavioral drift.

Recommended patterns: SOLID, Command/Query (CQRS-style) as the baseline for the
core library's operations.

## Data Storage

SQLite as the embedded, primary database for catalog metadata, folder
structure, bookmarks, watchlists, and deletion state. The design is open to
additional databases later if a need arises; SQLite is the starting point.

The API stores **only metadata and a path/content-hash reference**, never file
bytes. Markdown and text file content is edited in place on disk.

## Data Access

Undecided. The selection of an ORM, query builder, or raw driver layer will be
decided in Phase 2 and recorded in the
[Technology Stack Document](Technology%20Stack%20Document.md).

## Authentication

Two configurable modes, selected at startup; only one is active at runtime:

1. **External JWT** — JWTs issued by an external authentication microservice;
   Alexandria validates them through a pluggable auth service module. The actual
   provider integration is wired later.
2. **Local login** — encrypted credentials (email and a salted/hashed password)
   stored in an encrypted SQLite row on the local machine, verified by
   Alexandria. No plaintext credentials are ever stored.

Both modes authorize the single owner; only the configured mode is accepted.

## Testing

Rust's built-in `#[test]` framework with unit and integration tests in the
standard `cargo` layout. The full suite is run with:

```bash
cargo test
```

## External Dependencies

- **External authentication service** — issues and (via JWKS / shared secret)
  validates the JWTs Alexandria accepts.
- **The local filesystem** — indexing reads file metadata and content hashes;
  Markdown/text edits write back to the source path.

No email, payments, identity providers beyond the auth service, message
brokers, or object storage are involved.

## Deployment

Bundled with the Flutter desktop application, running on the user's own
machine. The HTTP server listens locally so the desktop client (and any other
local consumer) can reach it; clients that link the FFI core call it in
process. Concrete packaging specifics are recorded as undecided until the
desktop integration is built.
