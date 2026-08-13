# Design: Register the local account (F-09 — UC-41)

**Date:** 2026-08-13
**Status:** Approved, ready for implementation planning
**Tracks:** Extends the existing capability area — Pluggable Authentication (AU).
Milestone F-09.

## Context

Local mode already has the two operations F-09 shipped: UC-34 opens a session
from an email and password, and UC-35 sets or changes the singleton credential
row. There is no third operation for *creating* the account, because UC-35
doubles as one — its handler carries a conditional-authorization branch that
lets an unauthenticated caller through whenever no credential row exists yet.

That branch is the only place in the codebase where a handler's authorization
depends on stored state, and `set_credentials.rs` says so in a comment. It also
makes bootstrap indistinguishable from an overwrite at the API surface: the same
endpoint, the same body, the same `200`, whether the caller is creating the
owner's account or replacing it. A client that means "sign me up" cannot express
that intent, and cannot be told "an account already exists" — it just silently
succeeds at replacing credentials it should not have been able to touch.

This design adds an explicit registration operation and returns UC-35 to the
unconditional-authorization shape every other handler in the codebase has.

Alexandria stays single-owner. Nothing here relaxes BR-01 or introduces a
per-user foreign key: there is still exactly one credential row, `id = 1`. What
changes is that creating it is its own use case, and creating it twice is an
error.

## Decisions

1. **Registration is a distinct endpoint, not a flag on the existing one.**
   `POST /v1/auth/local/register` creates the account exactly once; `POST
   /v1/auth/local/credentials` changes an existing one and always requires
   authentication. Splitting the two lets each carry a single, honest
   authorization rule, and lets "already registered" be a real error instead of
   a silent overwrite. Uniqueness itself is enforced by an atomic
   create-if-absent write (`insert_if_absent`, `INSERT … ON CONFLICT (id) DO
   NOTHING`) at the storage layer, not merely by the earlier existence check —
   `get()` then `upsert()` is check-then-act, and two concurrent first-time
   registrations could both pass the check before either wrote.

2. **Registration returns a session.** Register mints a session the same way
   UC-34 does and returns its id, so a client is authenticated immediately
   without a second round-trip through login. Session minting is extracted out
   of `LocalLoginHandler` into a shared helper rather than duplicated — the TTL,
   the clock, and the expiry arithmetic must not drift between the two paths.

3. **A password strength policy, applied to both register and change.** The
   current rule is "non-empty", which is indefensible for the one credential
   that guards the entire catalog. The policy is length-first with no
   character-class requirements — those push users toward predictable
   substitutions without adding real entropy.

4. **Registration takes a confirmation field.** The owner's password is
   unrecoverable: there is no email delivery, no reset flow, and no second
   account to recover from. A typo at registration locks the owner out of their
   own catalog until they edit the database by hand. A confirmation field is
   cheap insurance at exactly the moment the risk exists. Change (UC-35) does
   not take one — by then the caller holds a valid session and a mistake is
   recoverable through that session.

5. **A new `DomainError::Conflict(String)`.** Today the only 409-mapped variant
   is `InvalidState`, which carries no message and renders as the fixed string
   `"invalid state"`. Registration has two distinct 409 conditions — wrong auth
   mode and account-already-exists — and a client that cannot tell them apart
   cannot show a useful message. `Conflict(String)` maps to `409` with its
   message, the way `InvalidInput(String)` already maps to `400` with its own.

## UC-41 — Register the local account

| Field | Value |
| --- | --- |
| **ID** | UC-41 |
| **Name** | Register the local account |
| **Actors** | Owner |
| **Description** | Create the single owner's local-login account when none exists, and open a session for the caller. |
| **Preconditions** | The active auth mode is local login; no local credentials exist. |
| **Postconditions** | The credential row holds the submitted email and a salted Argon2 hash of the password, and a Session exists whose id is returned to the caller. On a failure before the credential row is written, neither is created; AF-06 is the one exception — the credential row survives a failed session creation. |
| **Requirements** | FR-AU-05, FR-AU-06, FR-AU-08, FR-AU-09, FR-AU-10, FR-AU-11 |

**Main Flow**

1. The caller submits an email, a password, and a password confirmation.
2. The system confirms the active auth mode is local login.
3. The system confirms no local credentials exist yet.
4. The system validates the email format, the password against the strength
   policy, and that the confirmation matches the password.
5. The system salts and hashes the password (Argon2) and writes the credential
   row. Only the hash is stored; the plaintext is never persisted or logged.
6. The system creates a Session with an expiry `sessionTtlHours` in the future
   (configurable, default 24) and returns its id, exactly as UC-34 does.

**Alternative Flows**

| ID | Condition | Outcome |
| --- | --- | --- |
| AF-01 | The active auth mode is external JWT | The system rejects with an invalid-operation error. |
| AF-02 | Local credentials already exist | The system rejects with a conflict error; the stored credentials are left untouched. |
| AF-03 | The email format is invalid | The system rejects with an invalid-input error. |
| AF-04 | The password fails the strength policy | The system rejects with an invalid-input error naming the unmet rule; no plaintext is logged. |
| AF-05 | The confirmation does not match the password | The system rejects with an invalid-input error. |
| AF-06 | The credential row is written but the session cannot be created | The system returns the underlying error; the account exists, and the caller obtains a session through UC-34. |

The checks run in the order listed — mode, then existence, then the three input
checks. An unauthenticated caller therefore learns only whether an account
exists, which the conflict error tells them anyway; they never learn anything
about a stored password by varying the one they submit.

AF-06 is deliberately not a rollback. Both writes go to the same SQLite
database, but wrapping them in a transaction would require the credential and
session repository ports to share one, which no other command in the codebase
does. The failure is a disk or database error, the account it leaves behind is
exactly the account the caller asked for, and UC-34 completes the job.

## Password strength policy (FR-AU-11)

`auth::password::validate_strength(password, email) -> Result<(), DomainError>`,
called by both the register and the change handlers. The rules:

| Rule | Rationale |
| --- | --- |
| At least 12 characters | Length is the only strength lever that scales. |
| At most 128 characters | Argon2 cost is paid per byte; an unbounded password is a cheap way to make the server work hard. |
| Not entirely whitespace | `"            "` passes a naive length check. |
| Not a single repeated character | `"aaaaaaaaaaaa"` passes any length floor. Checked on characters, not bytes. |
| Not equal to the email, and does not contain the email's local part (case-insensitively) | The email is submitted in the same request and is the first thing an attacker guesses. |
| Not in the embedded common-password list | A small (~25 entry) `const` list of common passwords, compared case-insensitively. Every entry is at least 12 characters — anything shorter is already unreachable through the length floor, so a shorter entry would be dead weight. Not a full corpus check: that needs a downloaded breach dataset, which the Technology Stack Document's dependency discipline rules out. |

Each failure returns `InvalidInput` naming the unmet rule so a client can render
it. The message never echoes the password.

## UC-35 amendment

`SetLocalCredentialsHandler::set` drops its conditional-authorization branch and
calls `auth.authenticate(token)` unconditionally, before any other check. The
doc comment explaining the exception goes with it.

With no account, local mode cannot issue a session, so an unauthenticated
bootstrap attempt returns `401` — the correct answer, since UC-41 is now the way
to create an account. UC-35's AF-03 is rewritten to "The caller is not
authenticated → the system denies with an unauthorized error", with no
"and credentials already exist" qualifier. Its precondition drops "(or no
credentials exist yet)".

The strength policy (FR-AU-11) applies here too: changing a password to a weak
one is rejected with the same errors as registering with one.

**This is a breaking API change.** Any client that used `POST
/v1/auth/local/credentials` for first-time setup must switch to `POST
/v1/auth/local/register`. The README section that currently tells operators to
bootstrap through UC-35 is updated to point at UC-41.

## Interfaces

### HTTP

`POST /v1/auth/local/register`, registered outside the blanket `require_auth`
gate alongside `login` and `set_credentials`.

Request:

```json
{ "email": "owner@example.com", "password": "…", "passwordConfirmation": "…" }
```

Response `201 Created`:

```json
{ "success": true, "email": "owner@example.com", "sessionId": "…" }
```

Errors: `400` (AF-03, AF-04, AF-05, or a malformed body), `409` (AF-01, AF-02 —
distinguished by the message body), `500` (AF-06 and other database failures).

`201` rather than `200`: registration creates a resource, and it is the one
local-auth operation that can only ever succeed once. `login` and
`set_credentials` keep their `200`.

### FFI

`alexandria_auth_local_register(json_body: *const c_char) -> AuthJsonResult`,
following the existing `alexandria_auth_local_login` shape exactly — same JSON
in / JSON out contract, same `#[allow(unsafe_code)]` on `#[no_mangle]`, same
`map_auth_err`. Declared in `header.h` next to the other two.

`map_auth_err` gains an arm for `DomainError::Conflict(_)` returning a new
`AUTH_ERR_CONFLICT: c_int = 10`. The other domains' mappers already end in a
catch-all `_ =>` arm, so `Conflict` needs no change there; it falls through to
their existing `*_ERR_OTHER`.

This satisfies FR-AU-08 (dual-surface parity) for the new operation.

## Code changes

| File | Change |
| --- | --- |
| `alexandria-core/src/errors.rs` | Add `Conflict(String)` and a `DomainError::conflict()` constructor beside `config()` / `internal()`. |
| `alexandria-core/src/auth/password.rs` | Add `validate_strength` and the common-password `const` list. |
| `alexandria-core/src/auth/local.rs` | Add `LocalRegisterResult { success, email, session_id }`. Extract session minting from `LocalLoginHandler::login` into `issue_session(sessions, clock, ttl_hours) -> Result<Uuid, DomainError>`. Add `LocalCredentialRepository::insert_if_absent`, the atomic create-if-absent write that makes AF-02 authoritative. |
| `alexandria-core/src/auth/commands/register.rs` (new) | `RegisterLocalAccountHandler<CR, SR, C>` — generic over the credential repository, session repository, and clock, matching `LocalLoginHandler`. No `AuthService` dependency: registration is unauthenticated by definition. |
| `alexandria-core/src/auth/commands/login.rs` | Call `issue_session` instead of minting inline. |
| `alexandria-core/src/auth/commands/set_credentials.rs` | Unconditional `authenticate`; call `validate_strength`. |
| `alexandria-core/src/auth/commands/mod.rs` | Export `register`. |
| `alexandria-http/src/middleware/error.rs` | Map `Conflict(msg)` to `409` with `msg`. |
| `alexandria-http/src/routes/auth.rs` | `LocalRegisterRequest`, `register` handler. |
| `alexandria-http/src/lib.rs` | Route `/v1/auth/local/register`, outside `require_auth`. |
| `alexandria-core/src/services.rs` | Construct and expose `register_local_account_handler` beside `local_login_handler`. |
| `alexandria-ffi/src/lib.rs`, `src/header.h` | `alexandria_auth_local_register`, `AUTH_ERR_CONFLICT`, `map_auth_err` arm. |

No migration. The `local_login_credentials` and `sessions` tables are unchanged.

## Testing

Following the Testing Specification's split — core decision logic against
in-memory fakes, transport behavior in integration tests.

**Core unit tests** (`alexandria-core/tests/auth/register.rs`): the main flow
(row written, hash is not the plaintext, session id returned and valid); one
test per alternative flow AF-01 through AF-05; and AF-06 with a session
repository fake that errors, asserting the credential row survives.

**Strength validator** (`alexandria-core/tests/auth/password.rs`): a table
covering each rule's boundary — 11 vs 12 characters, 128 vs 129, all-whitespace,
password equal to the email, password containing the local part in a different
case, a common-password hit, and a passing case.

**Updated** `alexandria-core/tests/auth/set_credentials.rs`: the existing
"unauthenticated succeeds when no credentials exist" test is rewritten to assert
`Unauthorized`; a weak-password case is added.

**HTTP integration** (`alexandria-http/tests`): `201` and body shape on success;
that the returned `sessionId` authenticates a subsequent gated request; `409`
on a second registration with the stored credentials unchanged; `400` for each
input failure; and that `/register` is reachable without an `Authorization`
header.

**FFI** (`alexandria-ffi/tests`): success returns `AUTH_OK` with the same JSON
the HTTP surface returns, and a second call returns `AUTH_ERR_CONFLICT`.

## Documentation

| Document | Change |
| --- | --- |
| Use Case Specification | New UC-41 section after UC-40; traceability row; amended UC-35 precondition and AF-03. |
| System Requirements Document | FR-AU-10 (registration) and FR-AU-11 (strength policy); `POST /v1/auth/local/register` endpoint row; update the `/v1/auth/local/credentials` row to "change" only; note UC-41 as the creator of the credential row in the entity notes. |
| README | New F-09 backlog row for UC-41 (F-09 becomes 3 / 4 until it lands); update the setup prose that points at UC-35 for bootstrap. |
| Vision Document | No change — single-owner scope is unaffected. |

## Out of scope

- Multiple local accounts or any per-user data ownership. Alexandria remains
  single-owner (BR-01).
- Password reset, recovery codes, or email delivery. There is no mail transport
  in the stack and no second principal to authorize a reset.
- Account deletion. Nothing in the product needs it, and an account-less local
  deployment is already reachable by deleting the database.
- Rate limiting or lockout on the login endpoint. It is a real gap, but it
  belongs to UC-34 and applies to every endpoint, not to registration — which
  can succeed only once regardless.
- Any change to external JWT mode (UC-36).
