# Design: E-mail confirmation and password reset (issue #102)

**Date:** 2026-08-13
**Status:** Implemented
**Tracks:** Extends the existing capability area — Pluggable Authentication (AU).
Milestone F-09.

## Context

The core has no e-mail confirmation of any kind. `alexandria_auth_local_register`
creates an account and opens a session in one step, and `LocalRegisterResult` is
`{success, email, session_id}` — nothing on it, or on `LocalLoginResult`, says
whether the address has been confirmed. So an account is created in a state that
is neither confirmed nor unconfirmed: the concept does not exist.

Two consequences are live. The front-end's `emailConfirmed` DTO field defaults to
`true` because an account that *cannot* be confirmed must not be locked out of
its own catalog, which makes its catalog lock permanently inert. And the owner's
password is unrecoverable: registration takes a password, hashes it, and there is
no reset path, so a forgotten password is a lost catalog.

**Delivery is not built here.** Mail transport will be an external service that
is not yet integrated with this API. This design ships the complete flow —
state, tokens, expiry, resend, both reset halves, and both transport surfaces —
behind a mail port whose only implementation today refuses to send. The
front-end can therefore call every operation and handle every outcome now;
nothing actually reaches an inbox until the integration lands, and the API says
so in a way a caller can act on rather than pretending it sent.

## Decisions

1. **A mail port with an unconfigured default.** `MailSender` is a core port.
   The only implementation wired today is `UnconfiguredMailSender`, which never
   sends and reports `mail_not_configured`. Config gains
   `[mail] provider = "none"`; the external service becomes a second
   implementation behind the same port with no change to any handler.

   The port has two methods, not one. `send` delivers a message; `available`
   answers "can anything be delivered here" without one. The second exists
   because of decision 10: the password-reset request must answer identically
   for a registered and an unregistered address, and the first implementation
   of it did not — the registered address attempted a send and got `503` while
   the unregistered one returned early with `202`, which is exactly the yes/no
   the uniform answer exists to withhold. Asking `available` before anything
   address-specific happens closes it. An HTTP test caught this.

2. **Confirmation never gates anything in the core.** The core records and
   reports confirmation state; it does not refuse catalog operations for an
   unconfirmed address. Gating in the core while delivery cannot work would
   brick every install the moment this ships. The catalog lock stays the
   front-end's policy (its FR-AU-12), driven by the state the core now reports.

3. **A token that could not be sent leaves no trace.** Tokens are recorded
   before the send, so a message that does go out never carries a code that was
   never written down. If the send then fails, the row is deleted rather than
   left behind: a code nobody received must not be usable, and — this is the
   part that bit — it must not make the *next* resend answer `resend_too_soon`,
   hiding the real reason behind a wait that protects nothing.

4. **Registration reports delivery, it does not depend on it.** Register still
   creates the account and opens a session. It then mints a confirmation code
   and attempts to send it; a send failure is reported on the result, never
   rolled back — this is exactly UC-01 AF-06 ("the account is created but the
   core reports that the confirmation message could not be sent").
   `LocalRegisterResult` gains `emailConfirmed: false`,
   `confirmationSent: bool`, and `confirmationError: Option<String>` carrying
   the reason code when it is `false`.

5. **One token table, two purposes.** `auth_tokens(purpose, token_hash, email,
   created_at, expires_at, consumed_at)`. Confirmation and reset differ only in
   purpose, lifetime, and shape; two tables would duplicate the expiry and
   single-use logic that must not drift between them.

6. **Only the hash is stored.** The plaintext code or token exists in the
   outbound message and nowhere else, hashed on the way in and looked up by
   hash. A database read must not yield a working reset token — the same reason
   FR-AU-06 stores no plaintext password.

7. **Confirmation is a code; reset is a token.** The confirmation code is 8
   Crockford-base32 characters (~40 bits) — short enough to retype from a
   message, long enough that guessing it is not a strategy given single-use and
   the resend interval. The reset token is 32 random bytes, hex encoded, because
   it travels in a link and nothing retypes it. (Hex rather than the base64 this
   design first named: it is URL-safe with no encoding dependency, and the extra
   length is in a value nobody reads.) Lifetimes: 24 hours for confirmation,
   1 hour for reset, both configurable.

8. **Three distinct failure reasons for a presented token.** `invalid`,
   `already_used`, and `expired` are separate reason codes, per the front-end's
   requirement. Distinguishing them leaks nothing an attacker who holds the
   token does not already have, and "expired — ask for a new one" is a
   materially different instruction than "that code is wrong".

9. **Resend is rate-limited and says so.** A resend within
   `auth.resend_interval_seconds` (default 60) of the last send is refused with
   its own outcome — `DomainError::TooManyRequests`, `429` over HTTP,
   `AUTH_ERR_RATE_LIMITED` over FFI — carrying `retryAfterSeconds` in `params`.
   A refusal is not an error the owner caused; it needs to be told apart from a
   real failure so the front-end can show a countdown instead of an alarm.

10. **Requesting a reset does not reveal whether the address is registered.**
   The response is the same `{"success": true}` whether or not the submitted
   address matches the owner's. A *transport* failure is different: when the
   mail port is unconfigured, that is a property of the installation, not of the
   address, so it returns `503` with `mail_not_configured` and leaks nothing.

11. **Completing a reset replaces the credentials and invalidates sessions.**
    The new password goes through the same `validate_strength` policy as
    registration, takes a confirmation field for the same reason registration
    does, consumes the token, and deletes every existing session — a password
    reset is what you do when you believe someone else may hold your
    credentials, so leaving live sessions open defeats it.

12. **Confirming does not require a session.** The code proves possession of the
    address, which is the whole point; requiring a session as well would stop an
    owner confirming from the device that received the message. Resend *does*
    require one — it takes no address, so it needs an authenticated caller to
    know whom to send to, and that also stops it being an open mail relay.

## Operations

| Operation | HTTP | FFI |
| --- | --- | --- |
| Report account state | `GET /v1/auth/local/account` | `alexandria_auth_local_account` |
| Confirm the address | `POST /v1/auth/local/email/confirm` | `alexandria_auth_local_confirm_email` |
| Resend the confirmation | `POST /v1/auth/local/email/resend` | `alexandria_auth_local_resend_confirmation` |
| Request a password reset | `POST /v1/auth/local/password/reset` | `alexandria_auth_local_request_password_reset` |
| Complete a password reset | `POST /v1/auth/local/password/reset/complete` | `alexandria_auth_local_complete_password_reset` |

**Report account state** — authenticated. `200 {"email": …, "emailConfirmed": bool}`.
This is the one the front-end's catalog lock reads.

**Confirm** — unauthenticated, body `{"code": …}`. `200 {"success": true,
"email": …, "emailConfirmed": true}`. `400` with `code = confirmation_invalid |
confirmation_already_used | confirmation_expired`. Confirming an
already-confirmed account is `200` and idempotent when the code is the one that
confirmed it, `already_used` otherwise.

**Resend** — authenticated, empty body. `200 {"success": true, "sent": true}`.
`429` `resend_too_soon` with `retryAfterSeconds`. `409` when the address is
already confirmed. `503` `mail_not_configured` when the port cannot send —
today, always.

**Request reset** — unauthenticated, body `{"email": …}`. `202 {"success": true}`
regardless of whether the address matches. `503` `mail_not_configured`.

**Complete reset** — unauthenticated, body `{"token", "password",
"passwordConfirmation"}`. `200 {"success": true, "email": …}`. `400` with
`code = reset_invalid | reset_already_used | reset_expired`, or any
`validate_strength` code from issue #101, or `password_confirmation_mismatch`.

`LocalLoginResult` and `LocalRegisterResult` both gain `emailConfirmed`, so a
client learns the state on the call it already makes.

## Schema

Migration `00000000000012_email_confirmation.sql`:

```sql
ALTER TABLE local_login_credentials ADD COLUMN email_confirmed_at TEXT;

CREATE TABLE auth_tokens (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    purpose     TEXT    NOT NULL,          -- 'email_confirmation' | 'password_reset'
    token_hash  TEXT    NOT NULL,
    email       TEXT    NOT NULL,
    created_at  TEXT    NOT NULL,
    expires_at  TEXT    NOT NULL,
    consumed_at TEXT
);
CREATE UNIQUE INDEX idx_auth_tokens_hash ON auth_tokens (token_hash);
CREATE INDEX idx_auth_tokens_purpose ON auth_tokens (purpose, created_at);
```

Existing accounts get `email_confirmed_at = NULL` — unconfirmed. That is the
truthful state: nothing has ever confirmed them. Since the core gates nothing on
it (decision 2) and the front-end defaults its own field, no install breaks.

## Configuration

```toml
[auth]
confirmation_ttl_hours = 24
password_reset_ttl_minutes = 60
resend_interval_seconds = 60

[mail]
# The outbound mail provider. "none" — the default and, until the external
# service is integrated, the only value — never sends and reports
# `mail_not_configured` to the caller.
provider = "none"
from_address = ""
```

## Code changes

- `crates/alexandria-core/migrations/00000000000012_email_confirmation.sql`.
- `crates/alexandria-core/src/auth/mail.rs` — `MailSender`, `OutboundMail`,
  `UnconfiguredMailSender`.
- `crates/alexandria-core/src/auth/tokens.rs` — generation, hashing,
  `AuthTokenRepository` port + Sqlite implementation, `TokenPurpose`.
- `crates/alexandria-core/src/auth/local.rs` — `email_confirmed_at` on
  `LocalCredential`; `confirm_email`, `delete_all_sessions` on the repos; the
  result DTOs.
- `crates/alexandria-core/src/auth/commands/` — `account_status.rs`,
  `confirm_email.rs`, `resend_confirmation.rs`, `request_password_reset.rs`,
  `complete_password_reset.rs`; `register.rs` mints and sends.
- `crates/alexandria-core/src/{config,services,errors}.rs` — settings, wiring,
  `TooManyRequests(Rejection)` and `Unavailable(Rejection)`. The latter stands
  to `ServiceUnavailable` exactly as issue #101's `Rejected` stands to
  `InvalidInput`: same class, plus a stable reason code. `ServiceUnavailable`'s
  own message is still not echoed — which of an installation's dependencies is
  down is not a caller's business, whereas `mail_not_configured` is.
- `crates/alexandria-http/src/routes/auth.rs` + `routes/mod.rs` — five routes.
- `crates/alexandria-ffi/src/{lib.rs,header.h}` — five functions,
  `AUTH_ERR_RATE_LIMITED`, `AUTH_ERR_SERVICE_UNAVAILABLE`.

## Testing

Main flow plus every alternative flow, per the Testing Specification, with a
parity assertion per operation:

- Core unit tests against fakes: confirm with a good code; invalid, already-used,
  and expired codes; resend inside and outside the interval; resend when already
  confirmed; reset request for a matching and a non-matching address returning
  the identical body; complete reset with good, invalid, used, and expired
  tokens; a weak new password; sessions gone after a completed reset.
- Registration: `confirmationSent: false` with `mail_not_configured` and the
  account still created and a session still returned (UC-01 AF-06).
- Token storage: the plaintext never appears in the database.
- HTTP tests per endpoint; FFI smoke tests per function; parity for all five.

## Documentation

- System Requirements Document — `FR-AU-13` … `FR-AU-19` (confirmation state,
  confirm, resend + rate limit, reset request, reset completion, token storage,
  mail port), the endpoint table, the data-model section, and BR mappings.
- Operations & Infrastructure Document §4 — the four new configuration keys.
- `config.toml.example` — the same keys, documented in place.

## What shipped differently

Three things moved between this design and the code, all recorded above:
`MailSender` grew an `available` probe (decision 1), the reset token is hex
rather than base64 (decision 7), and unsent tokens are deleted (decision 3).

One consequence is worth stating plainly for whoever picks up the mail
integration. Because the only provider refuses every send, the HTTP and FFI
tests cannot obtain a real confirmation code or reset token: those tests seed
the token row a successful send would have written, and exercise the endpoint
from there. The transport-dependent success paths are proven at the core level
against a fake sender that succeeds. When the provider lands, the end-to-end
paths become testable for the first time and should be.

## Out of scope

- Actually sending mail. The port is here; the provider is the external
  service's integration, and until it lands every send reports
  `mail_not_configured`.
- Message templates and their localization — they belong with the provider.
- Gating catalog access on confirmation (decision 2).
- Confirming a *changed* address via UC-35. `set_credentials` clears
  `email_confirmed_at` when the address changes, which is the truthful state,
  but issuing a fresh confirmation from that path is a separate change.
