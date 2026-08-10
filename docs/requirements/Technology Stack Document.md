# Technology Stack Document — Alexandria

## 1. Purpose

This document is the **single source of truth for the technologies used to build
Alexandria** — the runtime platform, language, libraries, data storage,
cross-cutting concerns, and testing tools, together with the version each is
pinned to and the role it plays.

Every other document in this folder **references this document** for technical
choices instead of restating them, so that:

- The domain documents ([Vision](Vision%20Document.md),
  [System Requirements](System%20Requirements%20Document.md),
  [Use Case Specification](Use%20Case%20Specification%20Document.md)) stay focused
  on *what* the system does.
- The [Operations & Infrastructure Document](Operations%20%26%20Infrastructure%20Document.md)
  stays focused on the platform's structure and operations.
- The [Testing Specification Document](Testing%20Specification%20Document.md)
  stays focused on *how* to test.
- Technology versions and roles are maintained in exactly **one** place.

> **Rule:** when a technology choice changes, it changes here first. Other
> documents link to this one rather than duplicating the detail.

> **Version policy:** where a version is not yet pinned, it is recorded as
> **"latest stable at implementation time"**. That is a recorded policy, not a
> placeholder — the implementing engineer selects and records the concrete
> version when the work is done.

---

## 2. Platform & Language

| Concern | Choice | Notes |
| --- | --- | --- |
| Runtime | **tokio async runtime** | latest stable at implementation time |
| Language | **Rust (edition 2021)** | toolchain tracked to latest stable at implementation time |
| Language features | `#![deny(unsafe_code)]` applied project-wide | enforced in every crate so the whole workspace is memory-safe by audit |
| Workspace | **Cargo workspace**, three library/server crates + tests | see the [Operations & Infrastructure Document](Operations%20%26%20Infrastructure%20Document.md) §2.3 for layout |

---

## 3. Libraries

| Package | Version | Used by | Role |
| --- | --- | --- | --- |
| **tokio** | latest stable at implementation time | all crates | async runtime for indexing, HTTP, and DB I/O |
| **axum** | latest stable at implementation time | alexandria-http | HTTP/REST-JSON server framework, tower middleware, extractors |
| **tower** / **tower-http** | latest stable at implementation time | alexandria-http | middleware pipeline (logging, auth, error mapping, CORS) |
| **sqlx** | latest stable at implementation time | alexandria-core | async SQLite driver with compile-time SQL verification; migrations |
| **uuid** | latest stable at implementation time | alexandria-core | public UUID identifiers for entities (`Uuid` v4) |
| **serde** / **serde_json** | latest stable at implementation time | all crates | JSON serialization for the HTTP/REST contract and FFI boundary |
| **tracing** / **tracing-subscriber** | latest stable at implementation time | all crates | structured, span-aware logging |
| **anyhow** | latest stable at implementation time | all crates | error propagation across crate boundaries |
| **thiserror** | latest stable at implementation time | alexandria-core | typed domain error enums per command/query |
| **argon2** | latest stable at implementation time | alexandria-core (auth) | salted password hashing for local-login mode |
| **reqwest** | latest stable at implementation time | alexandria-core (auth) | fetches the external auth service's JWKS in external mode |
| **jsonwebtoken** | latest stable at implementation time | alexandria-core (auth) | JWT decode/verification for external-auth mode |
| **toml** | latest stable at implementation time | all crates | `config.toml` parsing with env-var overrides |
| **cbindgen** | latest stable at implementation time | alexandria-ffi (build) | generates the C header consumed by Flutter FFI |
| **ring** or **sha2** | latest stable at implementation time | alexandria-core | content hashing for indexed files (SHA-256) |
| **walkdir** | latest stable at implementation time | alexandria-core | recursive tree walk performed by the indexer (UC-01) |
| **futures-util** | latest stable at implementation time | alexandria-core | `buffer_unordered`, the bounded-concurrency combinator the index and re-index walks are built on (FR-FC-08) |

Blocking filesystem work — the tree walk, hashing, and every metadata parse in
§3.1 — is dispatched to Tokio's blocking pool via `spawn_blocking` rather than
run on a runtime worker. That is both what keeps reads answerable during a scan
(FR-FC-08) and what makes `indexing.concurrency` buy real parallelism instead of
interleaved waiting.

Input validation is **hand-written** per command handler (a `validate_*`
function beside the handler it guards, unit-tested against its own table of
rejected inputs) rather than derived from a validation crate. The rules are
cross-transport invariants — no leading/trailing whitespace, no NUL that would
truncate at the FFI boundary, byte-length caps — that both the HTTP and FFI
surfaces have to apply identically (NFR-09), and keeping them as plain
functions is what lets both call the same code.

There is no OpenAPI specification. The REST contract is defined by §5 of the
[System Requirements Document](System%20Requirements%20Document.md) and the
route-level documentation in `alexandria-http`; generating a machine-readable
spec is a possible future addition, not a current dependency.

### 3.1 Metadata extraction (FR-FC-25)

Prefilling a file's subtype metadata at first index needs one reader per family
of formats. Each is best-effort: a parse failure leaves the fields empty and
never fails the file's indexing.

| Package | Used for | Notes |
| --- | --- | --- |
| **lofty** | audio tags (title, artist, album, year, genre, track) | pure Rust, no system dependency |
| **kamadak-exif** | image EXIF (title, pixel dimensions) | raw EXIF dimensions; `Orientation` is not applied |
| **lopdf** | PDF metadata and page count | pure Rust |
| **epub** / **quick-xml** | EPUB metadata | EPUB is reflowable, so it never yields a page count |
| **zip** | comic archive metadata and page count (CBZ) | reads `ComicInfo.xml` when present |
| **ffmpeg-next** | video duration, resolution, container metadata | **the one system dependency in the graph** — needs the ffmpeg C dev libraries, `pkg-config`, and `clang` present at build time. See the README's Building section for the per-platform install. |

---

## 4. Data Storage

| Concern | Choice |
| --- | --- |
| Catalog database | **SQLite** — embedded, file-based, ships with the desktop bundle; stores catalog metadata, collections, bookmarks, watchlists, reading lists, deletion state, and local-login credentials (password stored as a salted Argon2 hash, never plaintext). |
| Connection configuration | path from `config.toml` / env override; a single connection pool sized for a single-user workload. **WAL journal mode**, so reads proceed against a snapshot while an indexing run writes (FR-FC-08); sqlx leaves the journal mode alone by default, so this is set explicitly at pool construction. Every explicit transaction begins `IMMEDIATE`: they all write, and a deferred transaction that reads first gets an un-retryable `SQLITE_BUSY` when it tries to upgrade under contention. |
| Foreign keys | enforced — sqlx sets `PRAGMA foreign_keys = ON` per connection. The subtype tables cascade from `files`; `watch_progress`, `reading_progress`, and the two `collection_id` columns declare no foreign key (SQLite cannot add one via `ALTER TABLE`), so their cleanup is explicit in the repositories. |
| Migrations | **sqlx migrate**; migrations live in `alexandria-core/migrations` and run at startup. |
| Same engine in tests | yes — every environment including tests uses an on-disk or in-memory SQLite database; see the [Testing Specification Document](Testing%20Specification%20Document.md). |

The API stores **only metadata and a path/content-hash reference**, never file
bytes. Markdown and text file content is edited in place on disk.

---

## 5. Data Access

| Concern | Choice | Version |
| --- | --- | --- |
| Database access | **sqlx** — async, compile-time-checked queries | latest stable at implementation time |
| Migrations | **sqlx migrate** | latest stable at implementation time |
| Naming convention | snake_case tables/columns; repository methods named after the Command/Query they back | — |

Access pattern: a **repository module per aggregate** in `alexandria-core`,
exposing async methods invoked by Command/Query handlers. The repository boundary
is what makes the domain testable — handlers depend on repository *traits*, so
unit tests substitute in-memory fakes (see the
[Testing Specification Document](Testing%20Specification%20Document.md) §6.2).

---

## 6. Cross-Cutting Technologies

| Concern | Technology | Version | How it is used |
| --- | --- | --- | --- |
| Input validation | hand-written `validate_*` functions in alexandria-core | — | one function per validated value, called by the handler and shared by both transports (see §3) |
| Logging | **tracing** / **tracing-subscriber** | latest stable at implementation time | structured span-aware logs; see the [Operations & Infrastructure Document](Operations%20%26%20Infrastructure%20Document.md) §4 |
| Authentication / authorization | pluggable auth module (external JWT via **jsonwebtoken**, local login via **argon2**) | latest stable at implementation time | selected at startup from config; exactly one mode active |
| Error / result model | typed domain errors via **thiserror**; **anyhow** at crate boundaries; `Result<T, E>` everywhere | latest stable at implementation time | commands/queries return `Result<T, DomainError>`; the HTTP layer maps `DomainError` to status codes |
| API documentation | System Requirements Document §5 + rustdoc on each route | — | the endpoint table is the contract; the FFI surface mirrors the same operations. No generated OpenAPI spec (see §3). |
| Configuration | **toml** + env overrides | latest stable at implementation time | `config.toml` read at startup; the `ALEXANDRIA_*` env namespace overrides keys |
| Content hashing | **sha2** (SHA-256) | latest stable at implementation time | per-file content hash stored at index time, refreshed on re-index |

---

## 7. Testing Technologies

These are the technologies mandated for tests. **How** they are applied (naming,
structure, coverage, the per-unit workflow) is defined in the
[Testing Specification Document](Testing%20Specification%20Document.md); this
section is the canonical list of the tools and versions.

| Concern | Technology | Version | How it is used |
| --- | --- | --- | --- |
| Test framework | Rust built-in `#[test]` | — (std) | unit and integration tests in the standard cargo layout |
| Test runner / SDK | **cargo test** | latest stable at implementation time | runs the full suite; parity tests gated by a test trait |
| Coverage | **cargo-tarpaulin** | latest stable at implementation time | optional line/branch coverage reporting |
| Mocking / test doubles | hand-written trait fakes; **mockall** for trait mocks only when needed | latest stable at implementation time | repository and auth-service traits faked in unit tests |
| Test data generation | factory helpers in the tests crate | — | builders for entities and commands |
| Integration dependencies | on-disk or in-memory SQLite; temp-dir filesystem fakes; auth service fake | — | see the Testing Specification §7.2 |
| HTTP/FFI parity | a parity test harness asserting identical results across both surfaces | — | built on top of the integration suite |

---

## 8. Version Summary

| Category | Package / Tool | Version |
| --- | --- | --- |
| Platform | tokio | latest stable at implementation time |
| Language | Rust (edition 2021) | latest stable at implementation time |
| HTTP | axum | latest stable at implementation time |
| HTTP middleware | tower / tower-http | latest stable at implementation time |
| Database driver | sqlx | latest stable at implementation time |
| UUIDs | uuid | latest stable at implementation time |
| Serialization | serde / serde_json | latest stable at implementation time |
| Logging | tracing / tracing-subscriber | latest stable at implementation time |
| Error handling | anyhow | latest stable at implementation time |
| Domain errors | thiserror | latest stable at implementation time |
| Validation | validator | latest stable at implementation time |
| Password hashing | argon2 | latest stable at implementation time |
| JWT | jsonwebtoken | latest stable at implementation time |
| Configuration | toml | latest stable at implementation time |
| FFI headers | cbindgen | latest stable at implementation time |
| Content hashing | sha2 | latest stable at implementation time |
| API docs | utoipa | latest stable at implementation time |
| Coverage | cargo-tarpaulin | latest stable at implementation time |
| Mocking | mockall | latest stable at implementation time |