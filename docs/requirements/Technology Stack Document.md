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
| **validator** | latest stable at implementation time | alexandria-core | declarative input validation (entities, commands) |
| **argon2** | latest stable at implementation time | alexandria-core (auth) | salted password hashing for local-login mode |
| **jsonwebtoken** | latest stable at implementation time | alexandria-core (auth) | JWT decode/verification for external-auth mode |
| **toml** | latest stable at implementation time | all crates | `config.toml` parsing with env-var overrides |
| **cbindgen** | latest stable at implementation time | alexandria-ffi (build) | generates the C header consumed by Flutter FFI |
| **ring** or **sha2** | latest stable at implementation time | alexandria-core | content hashing for indexed files (SHA-256) |

---

## 4. Data Storage

| Concern | Choice |
| --- | --- |
| Catalog database | **SQLite** — embedded, file-based, ships with the desktop bundle; stores catalog metadata, collections, bookmarks, watchlists, reading lists, deletion state, and local-login credentials (encrypted). |
| Connection configuration | path from `config.toml` / env override; a single connection pool sized for a single-user workload. |
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
| Input validation | **validator** | latest stable at implementation time | declarative constraints on command structs and entity fields |
| Logging | **tracing** / **tracing-subscriber** | latest stable at implementation time | structured span-aware logs; see the [Operations & Infrastructure Document](Operations%20%26%20Infrastructure%20Document.md) §4 |
| Authentication / authorization | pluggable auth module (external JWT via **jsonwebtoken**, local login via **argon2**) | latest stable at implementation time | selected at startup from config; exactly one mode active |
| Error / result model | typed domain errors via **thiserror**; **anyhow** at crate boundaries; `Result<T, E>` everywhere | latest stable at implementation time | commands/queries return `Result<T, DomainError>`; the HTTP layer maps `DomainError` to status codes |
| API documentation | **utoipa** (OpenAPI) | latest stable at implementation time | generates the OpenAPI spec for the REST contract; the FFI surface mirrors the same operations |
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