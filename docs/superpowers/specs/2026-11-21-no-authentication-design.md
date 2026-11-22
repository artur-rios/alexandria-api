# No authentication, pinned and favorite libraries, catalog statistics

**Date:** 2026-11-21
**Status:** approved
**Companion:** alexandria-ui `docs/superpowers/specs/2026-11-21-library-navigation-design.md`

## Why

Alexandria catalogs files on the machine it runs on, for the person sitting at it. It runs on Windows and Linux
against local files. Nothing connects to it over a network, so nothing justifies a login. In the owner's words, the
login "makes no sense".

The core carries a lot of machinery for something that has no reason to exist:

- Three authentication modes: its own account (local), Heimdall JWTs (external) and the Windows account (windows).
- A session table, recovery codes, password hashing and a Windows owner check at init.
- A `token` argument on nearly every FFI call.
- `alexandria-http`, a REST/JSON server of about 15,700 lines. Its only purpose is to let other programs reach a
  catalog over the network. Nothing releases it, and the app doesn't use it.

The app is also changing how it is navigated: libraries in the left menu, pinned and favorite libraries, and catalog
totals on the home screen. The core has to store the pins and favorites and answer the totals.

## Goals

- The core has no authentication: no modes, no accounts, no sessions, no recovery codes, no Windows owner check.
- No FFI call takes a token.
- `alexandria-http` is gone from the workspace, and so is everything that exists only for it.
- Existing catalogs upgrade cleanly. The migration deletes the stored credential hash, the sessions and the recovery
  codes.
- A library can be pinned and marked as a favorite. Both are stored in the catalog, and the library listing reports
  them.
- A library can be renamed at any time.
- One call answers the catalog totals: how many libraries there are, and how many files of each type.
- The core's docs (README, CONTRIBUTING, CHANGELOG, `docs/requirements`, `docs/initial`,
  `docs/System Behavior Document.md`, `config.toml.example`) describe a local, login-free core with only a C ABI.
- The release is alexandria-api 0.7.0. It breaks the FFI, which is allowed below 1.0.

## Non-goals

- No replacement access control of any kind (no PIN, no OS check, no file permissions on the catalog beyond what the
  OS gives the user's own folder).
- No ordering of libraries in the core. Pinned-first and A–Z are the app's presentation. The core reports the flags.
- No per-library statistics. The totals cover the whole catalog.
- No rewrite of the past design specs under `docs/superpowers/specs`. They are a record of what was decided at the
  time.

## Anti-goals

- An upgrade from 0.6 must not fail or lose catalog data. Only the authentication records are removed.
- The FFI must not keep `token` parameters that are ignored. A parameter that means nothing invites a caller to
  think it does.
- The remaining result codes must keep their numbers. The app maps codes by value, and renumbering them breaks it
  silently.

## Approaches

The owner picked **A**.

- **A. Remove the HTTP server along with all authentication.** The core becomes what the app actually uses: a
  library called through the C ABI. Pins, favorites and totals are added in one place. The cost is that no program
  other than the app can talk to a catalog, which is already true in practice.
- **B. Keep the server without a login, reachable only from localhost.** Scripts on the machine could still query
  it. The cost is that every new feature has to be added and tested there too, and any local program could change
  the catalog through it.

## Design

### 1. Authentication out of the core

Remove the following:

- `crates/alexandria-core/src/auth/` in full: `local`, `heimdall`, `jwks`, `password`, `recovery`,
  `windows_identity`, `commands/*` and `testdata`.
- `RuntimeAuthService` and every auth handler and service in `services.rs`. A handler that today checks a session
  or token before acting simply acts.
- `config.rs`: `AuthMode`, `AuthSettings` and `HttpSettings`; the `auth` and `http` fields of `Settings`; and every
  `ALEXANDRIA_AUTH_*` and `ALEXANDRIA_HTTP_*` environment override.
  - `Settings` keeps no `deny_unknown_fields`. An existing `config.toml` with `[auth]` or `[http]` tables still
    loads, and those tables are ignored.
- The auth-only error variants in `errors.rs`, along with anything else that has no caller once the above is gone.
- Crates in `Cargo.toml` that only these modules use (JWT, JWKS fetching, argon2 or the password hash crate,
  Windows identity APIs). The builder confirms each with `cargo machete` or by building without it.

### 2. The FFI

`crates/alexandria-ffi`:

- Remove every `alexandria_auth_*` function:
  - `local_login`
  - `windows_login`
  - `local_set_credentials`
  - `local_register`
  - `local_account`
  - `local_redeem_recovery_code`
  - `local_regenerate_recovery_codes`
- Also remove `AUTH_ERR_*`, and the result structs that only these functions return.
- Remove the `token` parameter from every other function, and keep the others in the same order. For example:
  - `alexandria_files_list(json_filters, token)` becomes `alexandria_files_list(json_filters)`.
  - A function whose only argument was the token takes none.
- `alexandria_index_init` drops the Windows owner check. Its remaining failures are invalid settings, a failed
  migration and a panic.
- The `*_ERR_UNAUTHORIZED = 2` constants go. Code 2 is retired in every family and not reused. The other codes keep
  their values.
- Regenerate the C header (`cbindgen`) and update the FFI's own tests and docs.

### 3. The HTTP server out of the workspace

- Delete `crates/alexandria-http`, and remove it from the workspace `members`.
- Its tests go with it, and so does `crates/alexandria-ffi/tests/parity.rs`, which compares every operation through
  HTTP and the FFI and can't exist without the server. (My call.) No behaviour may lose its only test:
  - Before deleting, the builder goes through the HTTP tests and the parity tests, and finds each behaviour that no
    core test or FFI smoke test covers. Auth-only and HTTP-only behaviour (status codes, headers, routing) don't
    count.
  - Each one found gets a test in the core crate or in `crates/alexandria-ffi/tests/smoke.rs`, written before the
    deletion.
  - The pull request lists what was ported.
- Delete anything that exists only for the server: `tools/` scripts that start or test it, and HTTP-only sections of
  `config.toml.example`.
- `yggdrasil.yml` and the `Jenkinsfile` aren't expected to change, because the checks run workspace-wide. If one has
  to change, the pull request becomes the owner's to merge, under the repository's rule.

### 4. Migration `00000000000030_no_authentication.sql`

A transactional migration (not `-- no-transaction`):

```sql
DROP TABLE IF EXISTS sessions;
DROP TABLE IF EXISTS local_login_credentials;
DROP TABLE IF EXISTS recovery_codes;
DROP TABLE IF EXISTS auth_tokens;
DROP INDEX IF EXISTS idx_sessions_expires_at;

ALTER TABLE libraries ADD COLUMN pinned   INTEGER NOT NULL DEFAULT 0 CHECK (pinned   IN (0, 1));
ALTER TABLE libraries ADD COLUMN favorite INTEGER NOT NULL DEFAULT 0 CHECK (favorite IN (0, 1));
```

- `auth_tokens` was already dropped in migration 14. The `IF EXISTS` keeps the migration idempotent against a catalog
  in any state.
- The builder confirms that no table still has a foreign key to the dropped tables. Migrations 9, 12, 13 and 14 are
  the ones that created or touched them.
- Migrations 0–29 are left exactly as they are, because sqlx checksums them.

### 5. Pinned and favorite libraries

- `Library` (model) gains `pinned: bool` and `favorite: bool`. They are serialized as `pinned` and `favorite` in
  every JSON that carries a library: the libraries list, browse, register.
- Two commands, each with a handler, a repository method and an FFI function:
  - `alexandria_library_set_pinned(uuid: *const c_char, pinned: bool) -> c_int`
  - `alexandria_library_set_favorite(uuid: *const c_char, favorite: bool) -> c_int`
  - They return `0`, `LIBRARY_ERR_INVALID_INPUT` (a malformed UUID), `LIBRARY_ERR_NOT_INITIALIZED` or
    `LIBRARY_ERR_NOT_FOUND`.
  - Setting a flag to the value it already has succeeds and changes nothing.
- Moving, renaming or rescanning a library keeps its flags. Removing it removes them with it.

### 5a. Renaming a library

The owner wants a library to be renamable at any time. Today its name is fixed when it is added.

- A command with a handler, a repository method and an FFI function:
  `alexandria_library_rename(uuid: *const c_char, json_body: *const c_char) -> LibraryJsonResult`. It has the same
  shape as `alexandria_library_move`. The body is `{"name": "..."}`, and the answer is the updated library as JSON.
- The name is checked by `validate_library_name`, the rule register uses: a blank name is refused with
  `LIBRARY_ERR_INVALID_INPUT`, and the stored name is trimmed. Two libraries may share a name, as they may today.
- It is allowed at any time, including while the library's run is in progress or paused. The name is not part of
  anything a run reads, so nothing waits for the run.
- It also returns `LIBRARY_ERR_NOT_FOUND` for an unknown UUID, and `LIBRARY_ERR_NOT_INITIALIZED`. Renaming to the
  current name succeeds and changes nothing.

### 6. Catalog statistics

- One query and one FFI function: `alexandria_catalog_stats_json() -> LibraryJsonResult`. It uses the same
  `{status, json}` struct the library calls return, and the `LIBRARY_ERR_*` codes: `0`,
  `LIBRARY_ERR_NOT_INITIALIZED` or `LIBRARY_ERR_OTHER`. The caller frees `json` with `alexandria_free_string`.
  (My call: a struct rather than a bare pointer, so the app can tell "not initialized" from a failed query. The
  `LIBRARY_` family because the totals are about libraries and their files.) The body:

  ```json
  { "libraries": 3,
    "filesByType": { "audio": 120, "video": 40, "html": 0, "text": 12,
                     "document": 30, "comic": 8, "image": 500 } }
  ```

- `filesByType` has every `FileType`, using the names `FileType::as_str` gives, with `0` for an empty type, so a
  client never has to tell "none" from "missing".
- It counts files the catalog lists: those not soft-deleted. Whether files marked missing count is decided to match
  what the listings show. The builder records the choice in the function's doc comment.

### 7. Tests

Failing tests first, in the crates' existing style:

- Migration: a 0.6 catalog with a credential, sessions and recovery codes migrates. The tables are gone, every
  library reads `pinned = 0` and `favorite = 0`, and no file, run or library is lost.
- Settings: a `config.toml` with `[auth]` and `[http]` tables still loads.
- Pin and favorite: set, unset, repeated set, unknown UUID (`NOT_FOUND`), malformed UUID (`INVALID_INPUT`), the flags
  surviving a move and a rescan, and the flags in the list and browse JSON.
- Rename tests cover:
  - a new name, which is stored trimmed;
  - a blank name, which is refused with `INVALID_INPUT` and stores nothing;
  - an unknown UUID, which answers `NOT_FOUND`;
  - a rename while the library's run is in progress, which succeeds and leaves the run untouched;
  - the pin and favorite flags, which are kept;
  - the new name, which shows in the list and browse JSON.
- Statistics: an empty catalog (zero libraries, every type `0`), mixed types across two libraries, and soft-deleted
  files not counted.
- FFI: the existing tests updated for the token-free signatures. No test is left that asserts an `UNAUTHORIZED`
  answer.
- `ci-check lint-and-test` passes (clippy, fmt, the tests).

### 8. Docs

- README: drop the HTTP surface and "pluggable auth" from the summary, and the auth rows from the roadmap and
  backlog. Say the core is used through its C ABI.
- CONTRIBUTING: drop anything about running or testing the server.
- CHANGELOG `[Unreleased]`:
  - Removed: authentication (all three modes), the HTTP server, and the `token` parameters.
  - Added: pinned and favorite libraries, renaming a library, and catalog statistics.
  - Changed: the migration deletes stored credentials.
- `docs/requirements`:
  - Mark the authentication requirements (FR-AU-*) and the HTTP-only requirements removed. The use cases for
    registration, login, recovery codes, credentials change, Heimdall and Windows login are marked withdrawn, with
    the reason, rather than deleted. This follows how earlier removals were recorded there.
  - Add the requirements and use cases for pin, favorite, rename and statistics.
- `docs/initial` and `docs/System Behavior Document.md`: the same, where they describe auth or the server.

## Left to the builder

- The exact names of the new handlers, repository methods and the statistics query.
- Whether the statistics query is one `GROUP BY` or one count per type.
- The requirement IDs for the new requirements, following the documents' numbering.
- Which crates become unused, as confirmed by the build.
