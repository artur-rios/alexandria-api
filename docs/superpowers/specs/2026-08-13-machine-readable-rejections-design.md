# Design: Machine-readable rejection reasons across HTTP and FFI (issue #101)

**Date:** 2026-08-13
**Status:** Approved, ready for implementation
**Tracks:** Extends the existing capability areas — Pluggable Authentication (AU)
and dual-transport parity (FR-FC-24, FR-AU-08, NFR-09).

## Context

`AuthJsonResult::err` sets `json` to NULL and carries only a status code
(`crates/alexandria-ffi/src/lib.rs:3106`). Every reason an auth handler produces
is therefore discarded at the FFI boundary, while the same rejection is
descriptive over HTTP: `ApiError::into_response` maps
`DomainError::InvalidInput(msg)` to `400` with `msg` in the body
(`crates/alexandria-http/src/middleware/error.rs:19`).

The gap is widest on registration. `validate_strength`
(`crates/alexandria-core/src/auth/password.rs:89`) produces six distinct
reasons, and `validate_email` four more; all ten reach an FFI caller as
`AUTH_ERR_INVALID_INPUT = 1`, indistinguishable from an unparseable body.

`AuthJsonResult`'s own doc comment claims `json` is "byte-for-byte the same
shape HTTP returns". That holds on success and not on failure, so a client
cannot be written once against both surfaces.

The front-end must render these to an owner in pt-BR and en. An English sentence
from the core is translatable by neither, so the reason has to travel as a
stable identifier plus the parameters that describe it.

## Decisions

1. **The error body gains a machine-readable reason, and both surfaces emit
   it.** The envelope stays `{"error": …}` and grows two optional members:

   ```json
   { "error": "password must be at least 12 characters",
     "code": "password_too_short",
     "params": { "min": "12" } }
   ```

   `error` remains the human-readable English fallback, unchanged for every
   caller reading it today. `code` is the stable identifier a client switches
   on; `params` carries the values the message interpolates, so a client can
   build its own sentence in its own language with the same bound the core
   enforced.

2. **`params` is a string map, not free-form JSON.** Every parameter this
   surface has is a bound or a name; a `BTreeMap<String, String>` keeps the
   shape predictable for an FFI caller parsing it by hand, and sorts
   deterministically so the two surfaces can be compared byte-for-byte in a
   parity test.

3. **A new `DomainError::Rejected(Rejection)`, alongside `InvalidInput`.**
   `InvalidInput(String)` has 104 construction sites outside tests; converting
   them all is a mechanical churn this issue does not need and would bury the
   real change. `Rejected` is `InvalidInput` plus a reason code, maps to the
   same `400` / `AUTH_ERR_INVALID_INPUT`, and is what every auth validator
   returns from now on. `InvalidInput` stays for the rest of the codebase and
   renders exactly as it does today — an omitted `code` means "no stable
   identifier for this one yet", which is honest and lets the rest of the
   surface adopt codes incrementally.

4. **The FFI error path carries the body.** `AuthJsonResult::err` takes the
   same rendered body HTTP sends and puts it in `json`. `status` keeps its
   present meaning — it is the coarse class, `code` is the reason. Ownership
   does not change: the caller already frees `json` on every path, and it stays
   NULL only when the library was never initialized, so there was no service to
   answer at all. A body this layer rejects before a handler sees it — an
   unreadable or unparseable `json_body` — carries `malformed_body`, the same
   code HTTP's extractor rejection carries.

5. **Rendering lives in the core, not in each surface.** A single
   `errors::error_body(&DomainError) -> (StatusCode-ish class, ErrorBody)` is
   the one place that decides what a `DomainError` looks like on the wire, so
   parity is a property of the code rather than of two `match` arms staying in
   step. The HTTP layer maps the class to an `axum::StatusCode`; the FFI layer
   maps it to an `AUTH_ERR_*` constant.

## The reason codes

Registration and credential change reject for these reasons. Codes are
`snake_case`, stable, and never reused for a different meaning.

| Code | Params | Raised by |
| --- | --- | --- |
| `email_required` | — | `validate_email` |
| `email_untrimmed` | — | `validate_email` |
| `email_malformed` | — | `validate_email` (missing/duplicate `@`, empty part, bad domain) |
| `password_too_short` | `min` | `validate_strength` |
| `password_too_long` | `max` | `validate_strength` |
| `password_whitespace` | — | `validate_strength` |
| `password_repeated_character` | — | `validate_strength` |
| `password_too_common` | — | `validate_strength` |
| `password_contains_email` | — | `validate_strength` (equal to, or containing, the address) |
| `password_confirmation_mismatch` | — | `RegisterLocalAccountHandler` |
| `malformed_body` | — | both surfaces, on an unparseable request body |

`email_malformed` deliberately collapses the four `validate_email` shapes into
one code: a client shows the same "that is not an e-mail address" either way,
and four codes would be four strings to translate for one user-visible outcome.
The English `error` still names which rule failed, for a log.

`password_contains_email` likewise covers both the equal-to and contains-the
cases — the remedy an owner needs is identical.

## Interfaces

### HTTP

No new endpoints, no status-code changes. Every `4xx`/`5xx` body may now carry
`code` and `params`; bodies for errors with no code are byte-identical to today's.

### FFI

No new functions and no new status codes. `AuthJsonResult.json` is non-NULL on
every failure that reached a handler, carrying the same bytes HTTP would send.
`header.h` is updated: the `AuthJsonResult` doc comment stops saying `json` is
NULL on failure and describes the error body instead.

## Code changes

- `crates/alexandria-core/src/errors.rs` — add `Rejection` (`code`, `message`,
  `params`) and `DomainError::Rejected`; add `error_body` plus an `ErrorClass`
  enum the two surfaces map from.
- `crates/alexandria-core/src/auth/password.rs` — `validate_strength` returns
  `Rejected` with the codes above.
- `crates/alexandria-core/src/auth/commands/set_credentials.rs` —
  `validate_email` likewise.
- `crates/alexandria-core/src/auth/commands/register.rs` — the confirmation
  mismatch likewise.
- `crates/alexandria-http/src/middleware/error.rs` — render via `error_body`.
- `crates/alexandria-http/src/middleware/auth.rs` — `invalid_input` becomes a
  `malformed_body` rejection.
- `crates/alexandria-ffi/src/lib.rs` — `AuthJsonResult::err` takes a body;
  `map_auth_err` renders through `error_body`.
- `crates/alexandria-ffi/src/header.h` — doc comment.

## Testing

Per the Testing Specification:

- Core unit tests: one per code in the table, asserting the code and its params
  (`password_too_short` carries `min = "12"`).
- HTTP tests: a weak-password registration returns `400` with
  `code = "password_too_short"`; an error with no code still renders
  `{"error": …}` and nothing else.
- FFI tests: the same registration returns `AUTH_ERR_INVALID_INPUT` **and** a
  non-NULL `json`.
- Parity test (`crates/alexandria-ffi/tests/parity.rs`): for the same rejected
  registration body, the FFI `json` bytes equal the HTTP response bytes.

## Documentation

- System Requirements Document — a new `FR-AU-12` requiring both surfaces to
  report a rejection's stable reason code and parameters, and an update to the
  error-envelope description.
- `docs/requirements` error-envelope table, where one exists.

## Out of scope

- Converting the 104 non-auth `InvalidInput` sites to codes. The mechanism
  lands here; adopting it elsewhere is incremental.
- Localizing anything in the core. The core emits English plus a code; the
  translation is the client's.
