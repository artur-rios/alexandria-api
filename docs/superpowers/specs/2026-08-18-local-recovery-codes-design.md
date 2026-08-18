# Design: Local recovery codes (F-09)

**Date:** 2026-08-18
**Status:** Approved, ready for implementation planning
**Tracks:** Replaces an existing capability area — Pluggable Authentication (AU).
Milestone F-09.

## Context

Local mode currently recovers a forgotten password by e-mail: `b9164ad` added
account confirmation state, a token table, a mail port, and the four operations
FR-AU-13 … FR-AU-19 describe — confirm an address, resend a confirmation,
request a password reset, complete one.

None of it has ever delivered a message. `MailProvider` has exactly one variant,
`None`, and every send is refused with `mail_not_configured`. So on every install
in existence `email_confirmed_at` is NULL, `auth_tokens` holds nothing that was
ever received, and a forgotten password still means a lost catalog. The machinery
is complete and inert, waiting on a mail integration this project does not want
to take on.

Recovery codes remove the dependency instead of satisfying it. The system hands
the owner ten codes when they register; any one of them, later, sets a new
password. There is no address to verify, no message to send, and no provider to
integrate — which is what makes this the *only* recovery method rather than one
of two.

The e-mail address stays as the login identifier. It is simply never verified
and never written to.

Alexandria stays single-owner. Nothing here relaxes BR-01: one credential row,
one account, and now one set of recovery codes belonging to it.

## Decisions

1. **E-mail confirmation is removed along with password reset, not kept.**
   Confirming an address proves control of a mailbox. That proof is worth
   something only if the system later writes to the mailbox — to recover an
   account, to notify, to alert. After this change it never does. Keeping
   confirmation would mean maintaining a mail port, a token table, an expiry
   policy, a resend interval, and four requirements in order to establish a fact
   nothing consumes.

2. **A recovery code is redeemed together with a new password, in one call.**
   The alternative — a code that opens a session, followed by an ordinary
   credentials change — leaves a window in which someone holding a found code
   has full catalog access and the owner has no signal that anything happened.
   Redeeming atomically replaces the password and deletes every session, so
   using a code is never silent: whoever did it must set a password, and
   everyone who was logged in is logged out.

   Redemption takes no e-mail address. There is one account, so an address would
   be a second copy of a fact the system already holds.

   Redemption is unauthenticated, necessarily: the person redeeming a code is
   the person who cannot log in.

3. **Ten codes, generated at registration, shown exactly once.**
   Ten because a recovery method that runs out on the second mistake is not one.
   Shown once because storing them retrievably would make the database a
   credential store for the credential that overrides the password — the
   database read that must not yield a working password must not yield a working
   recovery code either.

4. **Codes are stored as SHA-256 hashes, not Argon2.**
   Argon2's work factor exists to make guessing a *human-chosen* secret
   expensive. A recovery code is fifty bits chosen by the system, where the
   entropy does that job already: the search space, not the hash cost, is what
   makes guessing hopeless. Storing only the hash still matters, and for exactly
   the FR-AU-06 reason — a database read must not yield a working credential.

5. **Regeneration is part of the feature, not a follow-up.**
   Without it, recovery is finite: ten redemptions and the account is
   unrecoverable again, which is precisely the state this change exists to
   escape. It is also the only answer to a printed list going missing.
   Regenerating invalidates every existing code, used or not, and issues ten new
   ones — a partial refill would leave the owner unsure which of their written
   codes still work.

6. **The password is validated before the code table is touched.**
   A typo in the new password must not burn a recovery code. Checking the
   password policy first — a pure function over the input, no database read —
   means a rejected password leaves the codes untouched, and the owner can try
   again with the same code.

7. **An unknown code and an already-used code report different reasons.**
   This does leak that a presented code once existed. Guessing fifty bits is
   infeasible, so what it buys an attacker is nothing, while what it buys an
   owner is real: someone working down a printed list needs to know whether they
   have mistyped a code or already spent it. The tradeoff runs the other way
   from FR-AU-12's usual caution because the input here is high-entropy and
   system-generated, not a password.

## Architecture

Nothing about the shape of local mode changes: the same `AuthService`, the same
session model, the same singleton credential row. What changes is which commands
exist beside them.

### Removed

| Path | Reason |
| --- | --- |
| `auth/commands/confirm_email.rs` | Decision 1 |
| `auth/commands/resend_confirmation.rs` | Decision 1 |
| `auth/commands/request_password_reset.rs` | Replaced by redemption |
| `auth/commands/complete_password_reset.rs` | Replaced by redemption |
| `auth/mail.rs` | No outbound mail remains |
| `auth/tokens.rs` | No confirmation or reset token remains |
| `POST /v1/auth/local/email/confirm`, `…/email/resend`, `…/password/reset`, `…/password/reset/complete` | The operations are gone |
| `alexandria_auth_local_confirm_email`, `…_resend_confirmation`, `…_request_password_reset`, `…_complete_password_reset` | Same, over FFI (FR-AU-08) |
| `auth.confirmation_ttl_hours`, `auth.password_reset_ttl_minutes`, `auth.resend_interval_seconds`, the whole `[mail]` section, `MailProvider`, `MailSettings` | Nothing reads them |

### Added

**`auth/recovery.rs`** — generation, formatting, normalization, and hashing of a
recovery code. Sits beside `password.rs` and has the same shape: a small module
owning one credential's representation, with no repository and no clock, so the
format and the hash are testable as pure functions.

**`auth/commands/redeem_recovery_code.rs`** — decision 2's operation.
**`auth/commands/regenerate_recovery_codes.rs`** — decision 5's operation.

**`RecoveryCodeRepository`** — a port beside `LocalCredentialRepository` and
`SessionRepository`, with a SQLite implementation in `local.rs`'s style:

| Method | Purpose |
| --- | --- |
| `replace_all(hashes, created_at)` | Delete every existing code and insert this set. Serves both registration and regeneration; one method, because "the owner's codes are exactly these ten" is one idea. |
| `consume(hash, now)` | Mark the matching unconsumed code consumed, reporting which of three states it found: consumed now, already used, or absent. Decision 7 needs the three-way answer, and doing it in one call keeps find-then-write from racing itself. |
| `remaining()` | How many codes are unconsumed. |

### Changed

**`auth/commands/register.rs`** — generates the ten codes, writes their hashes in
the same operation that creates the credential row, and returns the plaintext in
its response in place of `confirmationSent` / `confirmationError`.

**`auth/commands/account_status.rs`** — reports the address and
`recoveryCodesRemaining` in place of the confirmed-state fields.

### Operations

| Operation | HTTP | Auth | Effect |
| --- | --- | --- | --- |
| Register | `POST /v1/auth/local/register` | none, once ever | Creates the account, opens a session, returns ten codes |
| Redeem | `POST /v1/auth/local/recovery/redeem` | none | Replaces the password, deletes every session, consumes one code |
| Regenerate | `POST /v1/auth/local/recovery/regenerate` | authenticated | Invalidates every code, returns ten new ones |
| Account | `GET /v1/auth/local/account` | authenticated | Address and remaining code count |

Each gets the matching FFI export, so both surfaces carry the same operations
(FR-AU-08).

### The code format

Ten characters drawn from Crockford base32 — the digits and upper-case letters
excluding `I`, `L`, `O` and `U`, so nothing on a printed list is ambiguous —
rendered in two groups of five, `XXXXX-XXXXX`. Roughly fifty bits.

Redemption upper-cases the input and strips hyphens and whitespace before
hashing, so a code can be typed as printed or as one run of characters, in
either case. The hyphen is presentation.

It also applies Crockford's substitution mapping — `O` to `0`, `I` and `L` to
`1` — the other half of excluding those letters. Leaving them out keeps a
printed code unambiguous; mapping them back handles the person who reads the
paper correctly and types the letter anyway. The mapping cannot collide,
because none of the three can occur in a generated code. (`U` is excluded for a
different reason and has no mapping.)

### Failure handling

Redemption checks in this order, and every rejection carries a stable reason
code (FR-AU-12):

1. Active mode is local — otherwise an invalid-operation rejection, as every
   other local operation already answers in external mode (FR-AU-03).
2. An account exists — otherwise a not-found rejection.
3. The new password satisfies FR-AU-11 and matches its confirmation — otherwise
   the existing password reason codes. **No code is consumed** (decision 6).
4. The code resolves to an unconsumed code — otherwise `recovery_code_unknown`
   or `recovery_code_used` (decision 7). This is where the code is spent: the
   consume is the check.
5. The password hash is replaced and every session is deleted.

Steps 4 and 5 go to the same database but not through one transaction, matching
UC-41 AF-06's existing precedent: the repository ports do not share a transaction
anywhere in this codebase. The code is consumed *before* the password is written,
and that order is deliberate. `consume` is not a separate bookkeeping write that
could be deferred — it is the atomic test-and-set that decides whether this
redemption is allowed at all, and its three-way answer is what distinguishes
`recovery_code_used` from `recovery_code_unknown`. Writing the password first
would mean two concurrent redemptions both overwrite it, with the loser told
`recovery_code_used` after it had already changed the password — contradicting
UC-43 AF-05, which says the password is unchanged in that case.

The residual risk is stated plainly: if `set_password_hash` fails after the code
is consumed, one code of ten is burned and nothing changed. The owner retries
with the next code. That is preferred to the alternative, where a failure leaves
the password already replaced by a request that was then rejected — a caller told
"no" while the account silently said "yes" is a far worse failure than a caller
told "no" who has nine codes left.

The new password is validated against the account's e-mail address
(`validate_strength(&new_password, &credential.email)`), which means an
unauthenticated caller can probe the owner's e-mail local part by watching which
passwords are rejected for containing it. This is accepted, not overlooked: the
service is single-user and binds to loopback by default (IR-03), the address is
also the login identifier the owner types anyway, and dropping the check would
weaken the password policy on exactly the path where a password is chosen without
proving knowledge of the old one.

## Requirements

FR-AU-13 … FR-AU-19 are withdrawn. Replacing them:

| ID | Requirement |
| --- | --- |
| FR-AU-13 | On registration the system shall generate ten single-use recovery codes, return them to the caller exactly once, and store only their hashes. |
| FR-AU-14 | The system shall replace the local password on presentation of an unconsumed recovery code together with a new password satisfying FR-AU-11, shall consume that code, and shall invalidate every existing session. |
| FR-AU-15 | The system shall reject a presented recovery code with a reason that distinguishes an unrecognised code from one already consumed. |
| FR-AU-16 | The system shall not consume a recovery code when the redemption fails for any other reason. |
| FR-AU-17 | The system shall, for an authenticated owner, replace every recovery code with ten new ones and return them exactly once. |
| FR-AU-18 | The system shall report to an authenticated owner how many recovery codes remain unconsumed. |
| FR-AU-19 | The system shall store only a hash of every recovery code; the plaintext shall exist only in the response that issues it. |

The IDs are reused rather than retired, and that is deliberate: these documents
describe the system as it is, not as it was, and leaving seven withdrawn numbers
as gravestones would make the next reader hunt for requirements that no longer
exist. Nothing outside this branch cites FR-AU-13 … FR-AU-19 — the citations are
all in the code and documents this change rewrites — so there is no reference to
strand. Where an ID is cited from somewhere this branch does not touch, it is
updated rather than left pointing at its old meaning.

Two new use cases join UC-41: **UC-43 Redeem a recovery code** and **UC-44
Regenerate recovery codes**. Both are specified in the Use Case Specification
Document with their alternative flows, and both appear in the traceability
table.

## Migration

Migration 13 drops `auth_tokens`, drops `local_login_credentials.email_confirmed_at`,
and creates `recovery_codes`:

| Column | Notes |
| --- | --- |
| `id` | INTEGER PRIMARY KEY AUTOINCREMENT |
| `code_hash` | TEXT NOT NULL UNIQUE — SHA-256 of the normalized code |
| `created_at` | TEXT NOT NULL |
| `consumed_at` | TEXT — NULL while unused |

Nothing of value is dropped. Every row `auth_tokens` could hold is a confirmation
or reset token that was never delivered, because the mail provider has always
been `None`; and `email_confirmed_at` is NULL on every install for the same
reason.

**Upgrade consequence:** an account registered before this migration has no
recovery codes. Its owner still knows their password, so the path is log in, then
regenerate. `GET /v1/auth/local/account` reporting `recoveryCodesRemaining: 0` is
what tells them to — a second reason that field exists.

## Testing

Per the [Testing Specification Document](../../requirements/Testing%20Specification%20Document.md)
§6, unit tests per handler against hand-written in-memory fakes, plus the
HTTP/FFI parity assertions §7.3 requires for each new operation.

| Case | Expected |
| --- | --- |
| Registration | returns exactly ten codes, all distinct |
| Registration | no stored value equals any returned code |
| Redeem, valid code | password replaced, that code consumed, every session deleted |
| Redeem, same code twice | second attempt rejected `recovery_code_used`, password unchanged |
| Redeem, unknown code | rejected `recovery_code_unknown` |
| Redeem, password fails FR-AU-11 | rejected with the password's own reason code, **no code consumed** |
| Redeem, confirmation mismatch | rejected, no code consumed |
| Redeem, lower-case / hyphenated / spaced / unhyphenated input | all accepted for the same code |
| Redeem, no account | not-found rejection |
| Regenerate | every prior code rejected afterwards, including unused ones; ten new codes returned |
| Regenerate, unauthenticated | unauthorized |
| Account | reports the remaining count, decreasing as codes are consumed |
| Account, pre-migration account | reports zero |
| Every operation, external mode active | invalid-operation rejection |
| Code format | ten characters, Crockford alphabet, no `I`/`L`/`O`/`U` |

## Out of scope

- **Windows credential login**, the remaining piece of the original request. It
  needs its own spec, chiefly to settle how a third authentication path coexists
  with FR-AU-01's single-active-mode rule.
- Any outbound mail, in any form. Removing the port is the point; adding a
  different notification channel is a separate decision.
- Rate-limiting redemption attempts. The API binds to loopback by default
  (IR-03) and a code is fifty random bits; a throttle here would guard a door
  nobody can find.
