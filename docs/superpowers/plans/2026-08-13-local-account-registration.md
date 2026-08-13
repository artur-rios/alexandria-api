# Local Account Registration (UC-41) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `POST /v1/auth/local/register` (UC-41), which creates the single owner's local account exactly once under a password strength policy and returns a session, and turn UC-35 into a change-only operation that always authenticates.

**Architecture:** Alexandria is a Rust workspace of three crates. `alexandria-core` holds the domain: Command handlers generic over trait "ports" (repositories, clock, auth service), so their decision logic is unit-tested against in-memory fakes with no database. `alexandria-http` (axum) and `alexandria-ffi` (C ABI, JSON in / JSON out) are thin transports over the same handler instances, wired once in `alexandria-core/src/services.rs`. Every use case must be reachable and behave identically from both transports (FR-AU-08, "parity"). This feature adds one handler, one route, one FFI export, and a shared password validator.

**Tech Stack:** Rust 2021, tokio, axum, sqlx + SQLite, argon2, chrono, uuid, serde/serde_json, thiserror, cbindgen (generates the C header at build time).

**Spec:** [`docs/superpowers/specs/2026-08-13-local-account-registration-design.md`](../specs/2026-08-13-local-account-registration-design.md). Read it before starting — it carries the reasoning behind every decision below.

## Global Constraints

- **Branch:** all work goes on `feature/uc-41-register-local-account`, cut from `main`. Never commit to `main`; never merge the PR yourself (Development Workflow §5 Step 7).
- **Single owner.** There is exactly one credential row, `id = 1`. Do not add per-user foreign keys, a users table, or any notion of a second account (BR-01).
- **No migration.** The `local_login_credentials` and `sessions` tables are unchanged. Do not add a file under `crates/alexandria-core/migrations/`.
- **Plaintext passwords are never stored and never logged** (FR-AU-06). This includes error messages: an error may name the rule a password broke, never the password.
- **Dual-surface parity** (FR-AU-08): every operation added to HTTP must also be added to FFI, returning the same JSON body.
- **`crates/alexandria-ffi/src/header.h` is generated and git-ignored.** `build.rs` regenerates it via cbindgen on `cargo build -p alexandria-ffi`, and `.gitignore` excludes it — it is not tracked. Never hand-edit it, and never `git add` it (the add would fail, or worse, start tracking a build artifact). Regenerate it to *verify* the exports are exposed correctly; that verification is the deliverable, not a commit.
- **Test naming:** `given_<condition>_when_<action>_then_<outcome>`, as every existing test in this repo does.
- **Full suite green before the PR:** `cargo test` from the workspace root (Development Workflow §6).
- **Password policy values, verbatim:** minimum 12 characters, maximum 128 characters.

## File Structure

| File | Responsibility |
| --- | --- |
| `crates/alexandria-core/src/auth/password.rs` | *(modify)* Argon2 hash/verify — gains `validate_strength` and its policy constants. |
| `crates/alexandria-core/src/auth/local.rs` | *(modify)* Local-auth types, ports, and SQLite adapters — gains `LocalRegisterResult` and the shared `issue_session` helper. |
| `crates/alexandria-core/src/auth/commands/register.rs` | *(create)* UC-41 handler. One file, one use case, matching `login.rs` / `set_credentials.rs`. |
| `crates/alexandria-core/src/auth/commands/login.rs` | *(modify)* Calls `issue_session` instead of minting inline. |
| `crates/alexandria-core/src/auth/commands/set_credentials.rs` | *(modify)* UC-35 becomes change-only. |
| `crates/alexandria-core/src/errors.rs` | *(modify)* Gains `DomainError::Conflict(String)`. |
| `crates/alexandria-core/src/services.rs` | *(modify)* Constructs and exposes `register_local_account_handler`. |
| `crates/alexandria-core/tests/common/mod.rs` | *(modify)* Gains `FailingSessionRepository`. |
| `crates/alexandria-core/tests/auth/{register,password}.rs` | *(create)* Unit tests, registered in `tests/auth.rs`. |
| `crates/alexandria-http/src/routes/auth.rs` | *(modify)* `LocalRegisterRequest` + `register` handler. |
| `crates/alexandria-http/src/lib.rs` | *(modify)* Route registration, outside the auth gate. |
| `crates/alexandria-http/src/middleware/error.rs` | *(modify)* `Conflict(msg)` → `409` with `msg`. |
| `crates/alexandria-ffi/src/lib.rs` | *(modify)* `alexandria_auth_local_register`, `LocalRegisterBody`, `AUTH_ERR_CONFLICT`. |

---

### Task 1: Password strength policy

A pure function with no I/O — start here so later tasks can call it. Nothing wires it up yet; it is `pub` in a library crate, so an unused `validate_strength` produces no dead-code warning.

**Files:**
- Modify: `crates/alexandria-core/src/auth/password.rs`
- Create: `crates/alexandria-core/tests/auth/password.rs`
- Modify: `crates/alexandria-core/tests/auth.rs`

**Interfaces:**
- Consumes: `DomainError::InvalidInput(String)` (already exists in `crates/alexandria-core/src/errors.rs`).
- Produces: `alexandria_core::auth::password::validate_strength(password: &str, email: &str) -> Result<(), DomainError>` — `Ok(())` when the password satisfies every rule. Also `MIN_PASSWORD_LENGTH: usize = 12` and `MAX_PASSWORD_LENGTH: usize = 128`.

- [ ] **Step 1: Create the branch**

```bash
git switch main && git pull && git switch -c feature/uc-41-register-local-account
```

- [ ] **Step 2: Register the new test module**

Add to the end of `crates/alexandria-core/tests/auth.rs`:

```rust
#[path = "auth/password.rs"]
mod password;
```

- [ ] **Step 3: Write the failing tests**

Create `crates/alexandria-core/tests/auth/password.rs`:

```rust
//! Unit tests for the FR-AU-11 password strength policy (UC-41 AF-04,
//! UC-35). Pure function, no fakes needed — a table of boundary cases per
//! rule.

use alexandria_core::auth::password::{
    validate_strength, MAX_PASSWORD_LENGTH, MIN_PASSWORD_LENGTH,
};
use alexandria_core::errors::DomainError;

const EMAIL: &str = "owner@example.com";

/// The message of an `InvalidInput`, or a panic naming what came back
/// instead. Every rejection in this policy is an `InvalidInput`.
fn rejection_message(result: Result<(), DomainError>) -> String {
    match result {
        Err(DomainError::InvalidInput(message)) => message,
        other => panic!("expected InvalidInput, got {other:?}"),
    }
}

#[test]
fn given_a_strong_password_when_validated_then_accepted() {
    assert!(validate_strength("correct horse battery", EMAIL).is_ok());
}

#[test]
fn given_a_password_one_char_below_the_floor_when_validated_then_rejected() {
    let password = "a".repeat(MIN_PASSWORD_LENGTH - 1);
    let message = rejection_message(validate_strength(&password, EMAIL));
    assert!(message.contains("at least"), "unexpected message: {message}");
}

#[test]
fn given_a_password_exactly_at_the_floor_when_validated_then_accepted() {
    // 12 distinct characters: long enough, and not a repeated run.
    assert!(validate_strength("abcdefghijkm", EMAIL).is_ok());
}

#[test]
fn given_a_password_one_char_above_the_ceiling_when_validated_then_rejected() {
    let password = "ab".repeat(MAX_PASSWORD_LENGTH); // 256 chars
    let message = rejection_message(validate_strength(&password, EMAIL));
    assert!(message.contains("at most"), "unexpected message: {message}");
}

#[test]
fn given_a_password_exactly_at_the_ceiling_when_validated_then_accepted() {
    let password = "ab".repeat(MAX_PASSWORD_LENGTH / 2); // 128 chars
    assert!(validate_strength(&password, EMAIL).is_ok());
}

#[test]
fn given_an_all_whitespace_password_when_validated_then_rejected() {
    let message = rejection_message(validate_strength(&" ".repeat(20), EMAIL));
    assert!(
        message.contains("whitespace"),
        "unexpected message: {message}"
    );
}

#[test]
fn given_a_single_repeated_character_when_validated_then_rejected() {
    let message = rejection_message(validate_strength("aaaaaaaaaaaaaaaa", EMAIL));
    assert!(
        message.contains("repeated"),
        "unexpected message: {message}"
    );
}

#[test]
fn given_the_email_as_the_password_when_validated_then_rejected() {
    let message = rejection_message(validate_strength(EMAIL, EMAIL));
    assert!(message.contains("email"), "unexpected message: {message}");
}

#[test]
fn given_the_email_local_part_in_another_case_when_validated_then_rejected() {
    // "owner" is the local part; embedding it uppercased must not evade.
    let message = rejection_message(validate_strength("xxOWNERxxxxxxx", EMAIL));
    assert!(message.contains("email"), "unexpected message: {message}");
}

#[test]
fn given_a_common_password_when_validated_then_rejected() {
    let message = rejection_message(validate_strength("Password1234", EMAIL));
    assert!(message.contains("common"), "unexpected message: {message}");
}

#[test]
fn given_a_short_email_local_part_when_validated_then_not_treated_as_a_substring() {
    // A one- or two-letter local part appears inside far too many good
    // passwords to reject on; only local parts of 3+ characters count.
    assert!(validate_strength("correct horse battery", "ab@example.com").is_ok());
}

#[test]
fn given_a_rejection_when_read_then_the_password_is_never_echoed() {
    let password = "aaaaaaaaaaaaaaaa";
    let message = rejection_message(validate_strength(password, EMAIL));
    assert!(
        !message.contains(password),
        "the message must never echo the password: {message}"
    );
}
```

- [ ] **Step 4: Run the tests to verify they fail**

```bash
cargo test -p alexandria-core --test auth password
```

Expected: FAIL to compile — `unresolved import ... validate_strength`, `MIN_PASSWORD_LENGTH`, `MAX_PASSWORD_LENGTH`.

- [ ] **Step 5: Implement the policy**

Append to `crates/alexandria-core/src/auth/password.rs`:

```rust
/// The password length floor (FR-AU-11, UC-41 AF-04). Length is the only
/// strength lever that scales; character-class rules ("one digit, one
/// symbol") push people toward predictable substitutions without adding
/// much real entropy, so this policy asks for length instead.
pub const MIN_PASSWORD_LENGTH: usize = 12;

/// The password length ceiling (FR-AU-11). Argon2 pays its cost per byte,
/// so an unbounded password is a cheap way to make the server work hard on
/// an endpoint that is deliberately reachable without authentication.
pub const MAX_PASSWORD_LENGTH: usize = 128;

/// Passwords common enough to be guessed early, compared case-insensitively.
///
/// Every entry is at least `MIN_PASSWORD_LENGTH` characters: anything
/// shorter is already unreachable through the length rule, so a shorter
/// entry here would be dead weight. Deliberately small — a real corpus
/// check needs a downloaded breach dataset, which the Technology Stack
/// Document's dependency discipline rules out. This is the place to add
/// long passwords that become common.
const COMMON_PASSWORDS: &[&str] = &[
    "password1234",
    "passwordpassword",
    "password123456",
    "123456789012",
    "1234567890123",
    "12345678901234",
    "qwertyuiop123",
    "qwerty1234567",
    "qwertyuiopasd",
    "administrator",
    "iloveyou1234",
    "letmein12345",
    "welcome123456",
    "changeme1234",
    "abcdefghijkl",
    "monkey1234567",
    "dragon1234567",
    "superman1234",
    "princess1234",
    "football1234",
    "baseball1234",
    "sunshine1234",
    "michael123456",
    "shadow1234567",
    "alexandria12",
];

/// The shortest email local part treated as a forbidden substring. A one-
/// or two-character local part ("jo@…") appears inside a great many
/// perfectly good passwords; rejecting on it would be noise, not security.
const MIN_LOCAL_PART_FOR_SUBSTRING_CHECK: usize = 3;

/// Check `password` against the strength policy (FR-AU-11), used by both
/// UC-41 registration and UC-35 change. `email` is the address submitted in
/// the same request — the first thing an attacker tries, so it must not be
/// the password or appear inside it.
///
/// Every rejection is an `InvalidInput` naming the unmet rule so a client
/// can render it. The message never echoes the password (FR-AU-06).
pub fn validate_strength(password: &str, email: &str) -> Result<(), DomainError> {
    let length = password.chars().count();
    if length < MIN_PASSWORD_LENGTH {
        return Err(DomainError::InvalidInput(format!(
            "password must be at least {MIN_PASSWORD_LENGTH} characters"
        )));
    }
    if length > MAX_PASSWORD_LENGTH {
        return Err(DomainError::InvalidInput(format!(
            "password must be at most {MAX_PASSWORD_LENGTH} characters"
        )));
    }

    if password.trim().is_empty() {
        return Err(DomainError::InvalidInput(
            "password must not be entirely whitespace".into(),
        ));
    }

    // A single character repeated passes any length floor. Checked on
    // characters, not bytes, so a repeated multi-byte character counts too.
    let mut chars = password.chars();
    let first = chars.next().expect("non-empty: length >= MIN_PASSWORD_LENGTH");
    if chars.all(|c| c == first) {
        return Err(DomainError::InvalidInput(
            "password must not be a single repeated character".into(),
        ));
    }

    let lowered = password.to_lowercase();
    if COMMON_PASSWORDS.contains(&lowered.as_str()) {
        return Err(DomainError::InvalidInput(
            "password is too common; choose a less predictable one".into(),
        ));
    }

    let email_lowered = email.to_lowercase();
    if !email_lowered.is_empty() && lowered == email_lowered {
        return Err(DomainError::InvalidInput(
            "password must not be the email address".into(),
        ));
    }
    let local_part = email_lowered.split('@').next().unwrap_or_default();
    if local_part.chars().count() >= MIN_LOCAL_PART_FOR_SUBSTRING_CHECK
        && lowered.contains(local_part)
    {
        return Err(DomainError::InvalidInput(
            "password must not contain the email address".into(),
        ));
    }

    Ok(())
}
```

- [ ] **Step 6: Run the tests to verify they pass**

```bash
cargo test -p alexandria-core --test auth password
```

Expected: PASS, 12 tests.

- [ ] **Step 7: Format and lint**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
```

Expected: no output from fmt, no warnings from clippy.

- [ ] **Step 8: Commit**

```bash
git add crates/alexandria-core/src/auth/password.rs crates/alexandria-core/tests/auth/password.rs crates/alexandria-core/tests/auth.rs
git commit -m "feat: add password strength policy (FR-AU-11)"
```

---

### Task 2: Extract shared session issuance

A pure refactor: `LocalLoginHandler` keeps behaving exactly as it does, but the session-minting arithmetic moves somewhere UC-41 can call it too. The existing login tests are the regression net — they must stay green without modification.

**Files:**
- Modify: `crates/alexandria-core/src/auth/local.rs`
- Modify: `crates/alexandria-core/src/auth/commands/login.rs:75-83`

**Interfaces:**
- Consumes: `SessionRepository`, `Clock`, `DomainError` (all already in the crate).
- Produces: `alexandria_core::auth::local::issue_session(sessions: &SR, clock: &C, ttl_hours: u32) -> Result<Uuid, DomainError>` where `SR: SessionRepository, C: Clock`. Task 3 calls this.

- [ ] **Step 1: Run the existing login tests to confirm they pass now**

```bash
cargo test -p alexandria-core --test auth login
```

Expected: PASS. This is the baseline the refactor must preserve.

- [ ] **Step 2: Add the shared helper**

Append to `crates/alexandria-core/src/auth/local.rs`:

```rust
/// Mint a session valid for `ttl_hours` from now and persist it
/// (FR-AU-09). Shared by UC-34 login and UC-41 registration: both open a
/// session on success, and the expiry arithmetic must not drift between
/// the two paths.
pub async fn issue_session<SR, C>(
    sessions: &SR,
    clock: &C,
    ttl_hours: u32,
) -> Result<Uuid, DomainError>
where
    SR: SessionRepository,
    C: Clock,
{
    let session_id = Uuid::new_v4();
    let now = clock.now();
    let expires_at = now + chrono::Duration::hours(i64::from(ttl_hours));
    sessions.create_session(session_id, now, expires_at).await?;
    Ok(session_id)
}
```

Add `use crate::catalog::clock::Clock;` to the imports at the top of `local.rs` if it is not already there.

- [ ] **Step 3: Call it from the login handler**

In `crates/alexandria-core/src/auth/commands/login.rs`, replace this block at the end of `login`:

```rust
        let session_id = Uuid::new_v4();
        let now = self.clock.now();
        let expires_at = now + chrono::Duration::hours(i64::from(self.session_ttl_hours));
        self.sessions
            .create_session(session_id, now, expires_at)
            .await?;
```

with:

```rust
        let session_id =
            issue_session(&self.sessions, &self.clock, self.session_ttl_hours).await?;
```

Then fix the imports at the top of the file: add `issue_session` to the `crate::auth::local` import list, and delete `use uuid::Uuid;` — the handler no longer names `Uuid` directly.

- [ ] **Step 4: Run the login tests to verify nothing changed**

```bash
cargo test -p alexandria-core --test auth login
```

Expected: PASS, same tests as Step 1.

- [ ] **Step 5: Format, lint, and run the whole suite**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test
```

Expected: no warnings; full suite green.

- [ ] **Step 6: Commit**

```bash
git add crates/alexandria-core/src/auth/local.rs crates/alexandria-core/src/auth/commands/login.rs
git commit -m "refactor: extract issue_session for reuse by registration"
```

---

### Task 3: The UC-41 registration handler

The core of the feature: the handler, its result type, and the `Conflict` error variant it needs. Unit-tested against in-memory fakes — no database, no HTTP.

**Files:**
- Modify: `crates/alexandria-core/src/errors.rs`
- Modify: `crates/alexandria-core/src/auth/local.rs`
- Create: `crates/alexandria-core/src/auth/commands/register.rs`
- Modify: `crates/alexandria-core/src/auth/commands/mod.rs`
- Modify: `crates/alexandria-core/tests/common/mod.rs`
- Create: `crates/alexandria-core/tests/auth/register.rs`
- Modify: `crates/alexandria-core/tests/auth.rs`

**Interfaces:**
- Consumes: `issue_session` (Task 2), `validate_strength` (Task 1), `validate_email` from `crate::auth::commands::set_credentials`, `hash_password`, `LocalCredentialRepository`, `SessionRepository`, `Clock`, `AuthMode`.
- Produces:
  - `alexandria_core::errors::DomainError::Conflict(String)` and `DomainError::conflict(impl Into<String>)`. Tasks 4 and 5 map it to `409` / `AUTH_ERR_CONFLICT`.
  - `alexandria_core::auth::local::LocalRegisterResult { success: bool, email: String, session_id: Uuid }`, serialized `camelCase` (`success`, `email`, `sessionId`).
  - `alexandria_core::auth::commands::register::RegisterLocalAccountHandler<CR, SR, C>` with `new(credentials: CR, sessions: SR, clock: C, mode: AuthMode, session_ttl_hours: u32)` and `async fn register(&self, email: String, password: String, password_confirmation: String) -> Result<LocalRegisterResult, DomainError>`.
  - `crate::common::FailingSessionRepository` in the core test harness.

- [ ] **Step 1: Register the new test module**

Add to the end of `crates/alexandria-core/tests/auth.rs`:

```rust
#[path = "auth/register.rs"]
mod register;
```

- [ ] **Step 2: Write the failing tests**

Create `crates/alexandria-core/tests/auth/register.rs`:

```rust
//! Unit tests for the UC-41 RegisterLocalAccountHandler (Testing
//! Specification §6). The handler runs against trait fakes — no real DB,
//! no auth service (registration is unauthenticated by definition).
//! Coverage: the main flow plus AF-01 … AF-06.

use chrono::{TimeZone, Utc};

use alexandria_core::auth::commands::register::RegisterLocalAccountHandler;
use alexandria_core::auth::local::{LocalCredentialRepository, SessionRepository};
use alexandria_core::auth::password::verify_password;
use alexandria_core::catalog::clock::FixedClock;
use alexandria_core::config::AuthMode;
use alexandria_core::errors::DomainError;

use crate::common::{
    FailingSessionRepository, FakeLocalCredentialRepository, FakeSessionRepository,
};

const EMAIL: &str = "owner@example.com";
const PASSWORD: &str = "correct horse battery";
const TTL_HOURS: u32 = 24;

fn clock() -> FixedClock {
    FixedClock(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap())
}

fn handler(
    credentials: FakeLocalCredentialRepository,
    sessions: FakeSessionRepository,
    mode: AuthMode,
) -> RegisterLocalAccountHandler<FakeLocalCredentialRepository, FakeSessionRepository, FixedClock> {
    RegisterLocalAccountHandler::new(credentials, sessions, clock(), mode, TTL_HOURS)
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_no_account_when_register_then_stores_the_hash_and_opens_a_session() {
    let credentials = FakeLocalCredentialRepository::new();
    let sessions = FakeSessionRepository::new();
    let h = handler(credentials.clone(), sessions.clone(), AuthMode::Local);

    let result = h
        .register(EMAIL.to_string(), PASSWORD.to_string(), PASSWORD.to_string())
        .await
        .expect("register");

    assert!(result.success);
    assert_eq!(result.email, EMAIL);

    let stored = credentials.get().await.unwrap().expect("credential stored");
    assert_eq!(stored.email, EMAIL);
    assert!(verify_password(PASSWORD, &stored.password_hash));
    assert_ne!(
        stored.password_hash, PASSWORD,
        "the plaintext must never be stored"
    );

    assert_eq!(sessions.count(), 1, "registration opens exactly one session");
    assert!(
        sessions
            .is_valid(result.session_id, clock().0)
            .await
            .unwrap(),
        "the returned session id must authenticate immediately"
    );
}

// ---------------- AF-01: wrong auth mode ----------------

#[tokio::test]
async fn given_external_auth_mode_when_register_then_conflict_and_nothing_written() {
    let credentials = FakeLocalCredentialRepository::new();
    let sessions = FakeSessionRepository::new();
    let h = handler(credentials.clone(), sessions.clone(), AuthMode::External);

    let err = h
        .register(EMAIL.to_string(), PASSWORD.to_string(), PASSWORD.to_string())
        .await
        .expect_err("must reject in external mode");

    assert!(matches!(err, DomainError::Conflict(_)), "got {err:?}");
    assert!(credentials.get().await.unwrap().is_none());
    assert_eq!(sessions.count(), 0);
}

// ---------------- AF-02: account already exists ----------------

#[tokio::test]
async fn given_an_existing_account_when_register_then_conflict_and_credentials_untouched() {
    let credentials = FakeLocalCredentialRepository::new();
    credentials
        .upsert(EMAIL, "existing-hash", clock().0)
        .await
        .unwrap();
    let sessions = FakeSessionRepository::new();
    let h = handler(credentials.clone(), sessions.clone(), AuthMode::Local);

    let err = h
        .register(
            "someone-else@example.com".to_string(),
            PASSWORD.to_string(),
            PASSWORD.to_string(),
        )
        .await
        .expect_err("must reject a second registration");

    assert!(matches!(err, DomainError::Conflict(_)), "got {err:?}");
    let stored = credentials.get().await.unwrap().expect("credential");
    assert_eq!(stored.email, EMAIL, "the stored email must be untouched");
    assert_eq!(stored.password_hash, "existing-hash");
    assert_eq!(sessions.count(), 0);
}

#[tokio::test]
async fn given_an_existing_account_and_a_weak_password_when_register_then_conflict_wins() {
    // Ordering matters: existence is checked before the input rules, so a
    // caller cannot probe stored state by varying the password.
    let credentials = FakeLocalCredentialRepository::new();
    credentials
        .upsert(EMAIL, "existing-hash", clock().0)
        .await
        .unwrap();
    let h = handler(credentials, FakeSessionRepository::new(), AuthMode::Local);

    let err = h
        .register(EMAIL.to_string(), "short".to_string(), "short".to_string())
        .await
        .expect_err("must reject");

    assert!(matches!(err, DomainError::Conflict(_)), "got {err:?}");
}

// ---------------- AF-03: invalid email ----------------

#[tokio::test]
async fn given_a_malformed_email_when_register_then_invalid_input_and_nothing_written() {
    let credentials = FakeLocalCredentialRepository::new();
    let sessions = FakeSessionRepository::new();
    let h = handler(credentials.clone(), sessions.clone(), AuthMode::Local);

    let err = h
        .register(
            "not-an-email".to_string(),
            PASSWORD.to_string(),
            PASSWORD.to_string(),
        )
        .await
        .expect_err("must reject a malformed email");

    assert!(matches!(err, DomainError::InvalidInput(_)), "got {err:?}");
    assert!(credentials.get().await.unwrap().is_none());
    assert_eq!(sessions.count(), 0);
}

// ---------------- AF-04: weak password ----------------

#[tokio::test]
async fn given_a_password_below_the_length_floor_when_register_then_invalid_input() {
    let credentials = FakeLocalCredentialRepository::new();
    let sessions = FakeSessionRepository::new();
    let h = handler(credentials.clone(), sessions.clone(), AuthMode::Local);

    let err = h
        .register(EMAIL.to_string(), "short".to_string(), "short".to_string())
        .await
        .expect_err("must reject a weak password");

    match err {
        DomainError::InvalidInput(message) => {
            assert!(message.contains("at least"), "unexpected: {message}");
            assert!(!message.contains("short"), "must not echo the password");
        }
        other => panic!("expected InvalidInput, got {other:?}"),
    }
    assert!(credentials.get().await.unwrap().is_none());
    assert_eq!(sessions.count(), 0);
}

// ---------------- AF-05: confirmation mismatch ----------------

#[tokio::test]
async fn given_a_mismatched_confirmation_when_register_then_invalid_input_and_nothing_written() {
    let credentials = FakeLocalCredentialRepository::new();
    let sessions = FakeSessionRepository::new();
    let h = handler(credentials.clone(), sessions.clone(), AuthMode::Local);

    let err = h
        .register(
            EMAIL.to_string(),
            PASSWORD.to_string(),
            "correct horse batteries".to_string(),
        )
        .await
        .expect_err("must reject a mismatched confirmation");

    assert!(matches!(err, DomainError::InvalidInput(_)), "got {err:?}");
    assert!(credentials.get().await.unwrap().is_none());
    assert_eq!(sessions.count(), 0);
}

// ---------------- AF-06: session creation fails after the write ----------------

#[tokio::test]
async fn given_session_creation_fails_when_register_then_errors_but_the_account_survives() {
    let credentials = FakeLocalCredentialRepository::new();
    let h = RegisterLocalAccountHandler::new(
        credentials.clone(),
        FailingSessionRepository,
        clock(),
        AuthMode::Local,
        TTL_HOURS,
    );

    let err = h
        .register(EMAIL.to_string(), PASSWORD.to_string(), PASSWORD.to_string())
        .await
        .expect_err("the session failure must surface");

    assert!(matches!(err, DomainError::Disk(_)), "got {err:?}");
    let stored = credentials.get().await.unwrap().expect("credential stored");
    assert_eq!(
        stored.email, EMAIL,
        "AF-06: the account exists; the caller obtains a session via UC-34"
    );
}
```

- [ ] **Step 3: Add the `FailingSessionRepository` fake**

Append to `crates/alexandria-core/tests/common/mod.rs`, next to `FakeSessionRepository`:

```rust
/// A `SessionRepository` whose writes always fail (UC-41 AF-06). Lets a
/// test drive the "credential row written, session creation failed" path
/// without a real database.
#[derive(Debug, Default, Clone, Copy)]
pub struct FailingSessionRepository;

impl SessionRepository for FailingSessionRepository {
    async fn create_session(
        &self,
        _id: Uuid,
        _created_at: DateTime<Utc>,
        _expires_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        Err(DomainError::Disk("session store unavailable".into()))
    }

    async fn is_valid(&self, _id: Uuid, _now: DateTime<Utc>) -> Result<bool, DomainError> {
        Err(DomainError::Disk("session store unavailable".into()))
    }
}
```

- [ ] **Step 4: Run the tests to verify they fail**

```bash
cargo test -p alexandria-core --test auth register
```

Expected: FAIL to compile — `unresolved import ... commands::register`.

- [ ] **Step 5: Add the `Conflict` error variant**

In `crates/alexandria-core/src/errors.rs`, add this variant to `enum DomainError`, immediately after `InvalidState`:

```rust
    /// A request that cannot be satisfied because it conflicts with state
    /// that already exists (UC-41 AF-01, AF-02). Distinct from
    /// `InvalidState`, which carries no message: registration has two
    /// different 409 conditions — wrong auth mode and account-already-
    /// exists — and a caller that cannot tell them apart cannot say
    /// anything useful to the owner.
    #[error("conflict: {0}")]
    Conflict(String),
```

And add this constructor to `impl DomainError`, beside `config` and `internal`:

```rust
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::Conflict(message.into())
    }
```

- [ ] **Step 6: Add the result type**

In `crates/alexandria-core/src/auth/local.rs`, add after `LocalLoginResult`:

```rust
/// Confirmation that the local account was created (UC-41 / FR-AU-10),
/// carrying the session id registration opened so the caller is
/// authenticated without a second round-trip through UC-34. Never carries
/// the password or its hash (FR-AU-06).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalRegisterResult {
    pub success: bool,
    pub email: String,
    pub session_id: Uuid,
}
```

- [ ] **Step 7: Write the handler**

Create `crates/alexandria-core/src/auth/commands/register.rs`:

```rust
use crate::auth::commands::set_credentials::validate_email;
use crate::auth::local::{issue_session, LocalCredentialRepository, LocalRegisterResult, SessionRepository};
use crate::auth::password::{hash_password, validate_strength};
use crate::catalog::clock::Clock;
use crate::config::AuthMode;
use crate::errors::DomainError;

/// UC-41 — Register the local account (FR-AU-10, FR-AU-11). Creates the
/// single owner's credential row when none exists and opens a session, so
/// the caller is authenticated immediately.
///
/// Takes no `AuthService`: registration is unauthenticated by definition —
/// it is what a caller does when there is nothing to authenticate with
/// yet. It is safe to leave ungated precisely because it can succeed only
/// once (AF-02); every subsequent call is a conflict.
///
/// Generic over the credential repository, session repository, and clock
/// so the decision logic is unit-tested against trait fakes, then wired
/// with the concrete Sqlite/System collaborators at runtime (services.rs).
pub struct RegisterLocalAccountHandler<CR, SR, C> {
    credentials: CR,
    sessions: SR,
    clock: C,
    mode: AuthMode,
    session_ttl_hours: u32,
}

impl<CR, SR, C> RegisterLocalAccountHandler<CR, SR, C>
where
    CR: LocalCredentialRepository,
    SR: SessionRepository,
    C: Clock,
{
    pub fn new(
        credentials: CR,
        sessions: SR,
        clock: C,
        mode: AuthMode,
        session_ttl_hours: u32,
    ) -> Self {
        Self {
            credentials,
            sessions,
            clock,
            mode,
            session_ttl_hours,
        }
    }

    /// Create the local account and return a session for it.
    ///
    /// The checks run in the order below — mode, then existence, then the
    /// three input rules. An unauthenticated caller therefore learns only
    /// whether an account exists, which AF-02's error tells them anyway;
    /// varying the submitted password never reveals anything about a
    /// stored one.
    pub async fn register(
        &self,
        email: String,
        password: String,
        password_confirmation: String,
    ) -> Result<LocalRegisterResult, DomainError> {
        // AF-01: the active auth mode must be local login.
        if self.mode != AuthMode::Local {
            return Err(DomainError::conflict(
                "local login is not the active auth mode",
            ));
        }

        // AF-02: registration creates the account; it never overwrites one.
        if self.credentials.get().await?.is_some() {
            return Err(DomainError::conflict("a local account already exists"));
        }

        // AF-03: the email must be well-formed.
        let email = validate_email(&email)?;
        // AF-04: the password must satisfy the strength policy.
        validate_strength(&password, &email)?;
        // AF-05: the owner's password is unrecoverable, so a typo here
        // would lock them out of their own catalog.
        if password != password_confirmation {
            return Err(DomainError::InvalidInput(
                "password confirmation does not match the password".into(),
            ));
        }

        let password_hash = hash_password(&password)?;
        self.credentials
            .upsert(&email, &password_hash, self.clock.now())
            .await?;

        // AF-06: if this fails the account still exists — deliberately not
        // rolled back. The two writes would need a shared transaction across
        // two repository ports, which no other command here does, and the
        // account left behind is exactly the one the caller asked for. They
        // obtain a session through UC-34.
        let session_id =
            issue_session(&self.sessions, &self.clock, self.session_ttl_hours).await?;

        Ok(LocalRegisterResult {
            success: true,
            email,
            session_id,
        })
    }
}
```

- [ ] **Step 8: Export the module**

In `crates/alexandria-core/src/auth/commands/mod.rs`, add:

```rust
pub mod register;
```

- [ ] **Step 9: Run the tests to verify they pass**

```bash
cargo test -p alexandria-core --test auth register
```

Expected: PASS, 8 tests.

- [ ] **Step 10: Format, lint, and run the whole suite**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test
```

Expected: green. If `alexandria-http`'s `middleware/error.rs` fails to compile with "non-exhaustive patterns: `DomainError::Conflict(_)` not covered", stop — that mapping is Task 4, Step 2. Do that step now rather than adding a placeholder arm.

- [ ] **Step 11: Commit**

```bash
git add crates/alexandria-core/src/errors.rs crates/alexandria-core/src/auth/local.rs crates/alexandria-core/src/auth/commands/register.rs crates/alexandria-core/src/auth/commands/mod.rs crates/alexandria-core/tests/common/mod.rs crates/alexandria-core/tests/auth/register.rs crates/alexandria-core/tests/auth.rs
git commit -m "feat: add UC-41 local account registration handler"
```

---

### Task 4: HTTP surface

Wire the handler into `Services`, expose it as `POST /v1/auth/local/register`, and map `Conflict` to `409`.

**Files:**
- Modify: `crates/alexandria-core/src/services.rs`
- Modify: `crates/alexandria-http/src/middleware/error.rs`
- Modify: `crates/alexandria-http/src/routes/auth.rs`
- Modify: `crates/alexandria-http/src/lib.rs`
- Create: `crates/alexandria-http/tests/auth_register_api.rs`

**Interfaces:**
- Consumes: `RegisterLocalAccountHandler`, `LocalRegisterResult`, `DomainError::Conflict` (Task 3).
- Produces:
  - `Services.register_local_account_handler: Arc<DefaultRegisterLocalAccountHandler>` — Task 5 calls it from FFI.
  - `POST /v1/auth/local/register`, body `{ "email", "password", "passwordConfirmation" }`, success `201` with `{ "success", "email", "sessionId" }`.

- [ ] **Step 1: Write the failing integration tests**

Create `crates/alexandria-http/tests/auth_register_api.rs`:

```rust
//! UC-41 integration tests for `POST /v1/auth/local/register` (Testing
//! Specification §7): the real axum router over a real temp SQLite
//! database. `common::test_app()` runs in local auth mode, so AF-01's
//! wrong-mode branch is unit-tested instead — there is no way to flip the
//! active mode mid-suite here.

mod common;

use alexandria_core::config::Settings;
use alexandria_http::app;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::common::test_app;

const PASSWORD: &str = "correct horse battery";

fn register_request(body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/auth/local/register")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn valid_body() -> Value {
    json!({
        "email": "owner@example.com",
        "password": PASSWORD,
        "passwordConfirmation": PASSWORD,
    })
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_no_account_when_register_posted_unauthenticated_then_201_with_session() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(register_request(valid_body()))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    assert_eq!(body["success"], true);
    assert_eq!(body["email"], "owner@example.com");
    assert!(
        body["sessionId"].as_str().is_some_and(|id| !id.is_empty()),
        "a session id must be returned: {body}"
    );
}

#[tokio::test]
async fn given_a_registration_session_when_used_on_a_gated_route_then_authenticated() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .clone()
        .oneshot(register_request(valid_body()))
        .await
        .expect("register one-shot");
    assert_eq!(response.status(), StatusCode::CREATED);
    let session_id = body_json(response).await["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    let gated = Request::builder()
        .method("GET")
        .uri("/v1/files")
        .header("authorization", format!("Bearer {session_id}"))
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(gated).await.expect("gated one-shot");

    assert_ne!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "the registration session must authenticate immediately"
    );
}

// ---------------- AF-02: already registered ----------------

#[tokio::test]
async fn given_an_existing_account_when_register_posted_then_409() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let first = router
        .clone()
        .oneshot(register_request(valid_body()))
        .await
        .expect("first one-shot");
    assert_eq!(first.status(), StatusCode::CREATED);

    let response = router
        .oneshot(register_request(json!({
            "email": "someone-else@example.com",
            "password": PASSWORD,
            "passwordConfirmation": PASSWORD,
        })))
        .await
        .expect("second one-shot");

    assert_eq!(response.status(), StatusCode::CONFLICT);
}

// ---------------- AF-03 / AF-04 / AF-05: input rules ----------------

#[tokio::test]
async fn given_a_malformed_email_when_register_posted_then_400() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(register_request(json!({
            "email": "not-an-email",
            "password": PASSWORD,
            "passwordConfirmation": PASSWORD,
        })))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn given_a_weak_password_when_register_posted_then_400() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(register_request(json!({
            "email": "owner@example.com",
            "password": "short",
            "passwordConfirmation": "short",
        })))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn given_a_mismatched_confirmation_when_register_posted_then_400() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(register_request(json!({
            "email": "owner@example.com",
            "password": PASSWORD,
            "passwordConfirmation": "correct horse batteries",
        })))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn given_a_body_missing_the_confirmation_when_register_posted_then_400() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(register_request(json!({
            "email": "owner@example.com",
            "password": PASSWORD,
        })))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
```

- [ ] **Step 2: Map `Conflict` to 409**

In `crates/alexandria-http/src/middleware/error.rs`, add this arm to the match, immediately after the `InvalidState` arm:

```rust
            DomainError::Conflict(msg) => (StatusCode::CONFLICT, msg.as_str()),
```

- [ ] **Step 3: Wire the handler into `Services`**

In `crates/alexandria-core/src/services.rs`:

Add to the imports beside the other auth command imports:

```rust
use crate::auth::commands::register::RegisterLocalAccountHandler;
```

Add beside `DefaultLocalLoginHandler`:

```rust
pub type DefaultRegisterLocalAccountHandler = RegisterLocalAccountHandler<
    SqliteLocalCredentialRepository,
    SqliteSessionRepository,
    SystemClock,
>;
```

Add to the `Services` struct beside `local_login_handler`:

```rust
    pub register_local_account_handler: Arc<DefaultRegisterLocalAccountHandler>,
```

In `build_services`, the existing `LocalLoginHandler::new(credential_repo, session_repo, …)` call *moves* both repositories. Change that call to clone them, then construct the new handler after it:

```rust
    let local_login_handler = Arc::new(LocalLoginHandler::new(
        credential_repo.clone(),
        session_repo.clone(),
        clock,
        settings.auth.mode,
        settings.auth.session_ttl_hours,
    ));
    let register_local_account_handler = Arc::new(RegisterLocalAccountHandler::new(
        credential_repo,
        session_repo,
        clock,
        settings.auth.mode,
        settings.auth.session_ttl_hours,
    ));
```

And add `register_local_account_handler,` to the returned `Services { … }` literal, beside `local_login_handler,`.

- [ ] **Step 4: Add the route handler**

In `crates/alexandria-http/src/routes/auth.rs`, add `LocalRegisterResult` to the `alexandria_core::auth::local` import, then append:

```rust
/// Request body for `POST /v1/auth/local/register` (UC-41). Unlike the
/// other two local-auth endpoints this carries a confirmation field: the
/// owner's password is unrecoverable, and a typo at registration locks
/// them out of their own catalog.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalRegisterRequest {
    pub email: String,
    pub password: String,
    pub password_confirmation: String,
}

/// `POST /v1/auth/local/register` — create the single owner's local
/// account and open a session for it (UC-41 / FR-AU-10, FR-AU-11).
/// Deliberately outside the blanket `require_auth` gate: there is nothing
/// to authenticate with before an account exists. Safe to leave ungated
/// because it succeeds only once — every later call is AF-02's conflict.
/// Returns `201` with the `LocalRegisterResult`, or `400` (malformed
/// email, weak password, mismatched confirmation, or a malformed body —
/// AF-03/AF-04/AF-05), or `409` (the active auth mode is not local, AF-01,
/// or an account already exists, AF-02 — distinguished by the message).
pub async fn register(
    State(state): State<AppState>,
    body: Result<Json<LocalRegisterRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<LocalRegisterResult>), ApiError> {
    let Json(request) =
        body.map_err(|err| invalid_input(format!("invalid register body: {err}")))?;

    let result = state
        .services
        .register_local_account_handler
        .register(
            request.email,
            request.password,
            request.password_confirmation,
        )
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::CREATED, Json(result)))
}
```

- [ ] **Step 5: Register the route**

In `crates/alexandria-http/src/lib.rs`, add the route to the ungated router beside the other two, and extend the comment above it:

```rust
    // `/health`, the local register, login, and credentials endpoints are
    // deliberately outside the gate: `/health` reports reachability to a
    // caller with no catalog credentials, registration is how the account
    // comes to exist at all (UC-41), and login is how a caller obtains
    // credentials in the first place (UC-34). Registration is safe ungated
    // because it succeeds only once (UC-41 AF-02); `/credentials` enforces
    // authentication in its own handler (UC-35).
    Router::new()
        .route("/health", get(routes::health::health))
        .route("/v1/auth/local/register", post(routes::auth::register))
        .route("/v1/auth/local/login", post(routes::auth::login))
        .route(
            "/v1/auth/local/credentials",
            post(routes::auth::set_credentials),
        )
```

- [ ] **Step 6: Run the integration tests to verify they pass**

```bash
cargo test -p alexandria-http --test auth_register_api
```

Expected: PASS, 7 tests. If `given_a_registration_session_when_used_on_a_gated_route_then_authenticated` fails because `/v1/files` is not the browse route in this codebase, check the route table in `crates/alexandria-http/src/lib.rs` and use whichever gated `GET` route is registered there — the assertion only cares that the response is not `401`.

- [ ] **Step 7: Format, lint, and run the whole suite**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test
```

Expected: green.

- [ ] **Step 8: Commit**

```bash
git add crates/alexandria-core/src/services.rs crates/alexandria-http/src/middleware/error.rs crates/alexandria-http/src/routes/auth.rs crates/alexandria-http/src/lib.rs crates/alexandria-http/tests/auth_register_api.rs
git commit -m "feat: expose UC-41 registration over HTTP"
```

---

### Task 5: FFI surface

Dual-transport parity (FR-AU-08): the same operation, the same JSON, over the C ABI.

**Files:**
- Modify: `crates/alexandria-ffi/src/lib.rs`
- Verify only (generated + git-ignored, never hand-edited, never committed): `crates/alexandria-ffi/src/header.h`
- Modify: `crates/alexandria-ffi/tests/parity.rs`

**Interfaces:**
- Consumes: `Services.register_local_account_handler` (Task 4).
- Produces: `alexandria_auth_local_register(json_body: *const c_char) -> AuthJsonResult` and `AUTH_ERR_CONFLICT: c_int = 10`.

- [ ] **Step 1: Write the failing parity test**

Append to `crates/alexandria-ffi/tests/parity.rs`, and add `alexandria_auth_local_register` to the `use alexandria_ffi::{…}` import list at the top of the file:

```rust
/// UC-41 parity: registering over HTTP and over FFI must produce the same
/// body shape, and a second registration must conflict on both surfaces.
#[tokio::test]
async fn given_uc41_register_when_called_on_both_surfaces_then_bodies_match() {
    const PASSWORD: &str = "correct horse battery";

    fn register_body() -> serde_json::Value {
        json!({
            "email": "owner@example.com",
            "password": PASSWORD,
            "passwordConfirmation": PASSWORD,
        })
    }

    // The FFI leg mutates process-global state (`services_slot`), so every
    // parity test in this file takes `SERIAL` first.
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let request = Request::builder()
        .method("POST")
        .uri("/v1/auth/local/register")
        .header("content-type", "application/json")
        .body(Body::from(register_body().to_string()))
        .unwrap();
    let response = router.clone().oneshot(request).await.expect("http register");
    assert_eq!(response.status(), axum::http::StatusCode::CREATED);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();

    let second = Request::builder()
        .method("POST")
        .uri("/v1/auth/local/register")
        .header("content-type", "application/json")
        .body(Body::from(register_body().to_string()))
        .unwrap();
    let second = router.oneshot(second).await.expect("http second register");
    assert_eq!(second.status(), axum::http::StatusCode::CONFLICT);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let (ffi_json, second_status): (String, i32) =
        tokio::task::spawn_blocking(move || -> (String, i32) {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(
                alexandria_index_init(cdb.as_ptr()),
                alexandria_ffi::INDEX_OK
            );

            let body = CString::new(register_body().to_string()).unwrap();
            let result = alexandria_auth_local_register(body.as_ptr());
            assert_eq!(result.status, alexandria_ffi::AUTH_OK, "ffi register failed");
            assert!(!result.json.is_null());
            let json = unsafe { CStr::from_ptr(result.json) }
                .to_str()
                .unwrap()
                .to_string();
            unsafe {
                alexandria_free_string(result.json);
            }

            let second = alexandria_auth_local_register(body.as_ptr());
            (json, second.status)
        })
        .await
        .unwrap();

    let ffi_body: serde_json::Value = serde_json::from_str(&ffi_json).unwrap();

    // ---- compare ----
    assert_eq!(
        second_status,
        alexandria_ffi::AUTH_ERR_CONFLICT,
        "a second FFI registration must conflict, as HTTP's 409 does"
    );
    assert_eq!(http_body["success"], ffi_body["success"]);
    assert_eq!(http_body["success"], json!(true));
    assert_eq!(http_body["email"], ffi_body["email"]);
    // Session ids are random per surface; assert shape, not equality.
    for body in [&http_body, &ffi_body] {
        let session_id = body["sessionId"].as_str().expect("sessionId");
        uuid::Uuid::parse_str(session_id).expect("sessionId must be a uuid");
    }
}
```

This mirrors the neighbouring UC-34/UC-35 parity test (search `given_same_local_credentials_when_set_and_logged_in_via_http_and_ffi_then_bodies_match` in the same file), which is where `SERIAL`, `db_path`, `local_settings`, `seed_session`, and `setup_ffi_db` all come from — they are already imported at the top of the file.

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test -p alexandria-ffi --test parity uc41
```

Expected: FAIL to compile — no `alexandria_auth_local_register`, no `AUTH_ERR_CONFLICT`.

- [ ] **Step 3: Add the error constant and mapping**

In `crates/alexandria-ffi/src/lib.rs`, beside the other `AUTH_ERR_*` constants:

```rust
/// UC-41 AF-01/AF-02: the request conflicts with existing state — the
/// active auth mode is not local, or an account already exists. The FFI
/// counterpart of HTTP's `409`.
pub const AUTH_ERR_CONFLICT: c_int = 10;
```

And add this arm to `map_auth_err`, before the catch-all:

```rust
        DomainError::Conflict(_) => AuthJsonResult::err(AUTH_ERR_CONFLICT),
```

The other domains' error mappers already end in a catch-all `_ =>` arm, so `Conflict` needs no change there.

- [ ] **Step 4: Add the request body and the export**

In `crates/alexandria-ffi/src/lib.rs`, beside `LocalCredentialsBody`:

```rust
/// Request body for `alexandria_auth_local_register` — the same JSON the
/// HTTP route takes: `{"email":"…","password":"…","passwordConfirmation":"…"}`.
#[derive(Debug)]
struct LocalRegisterBody {
    email: String,
    password: String,
    password_confirmation: String,
}

impl LocalRegisterBody {
    fn from_json_str(s: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(s).ok()?;
        let obj = value.as_object()?;
        Some(Self {
            email: obj.get("email")?.as_str()?.to_string(),
            password: obj.get("password")?.as_str()?.to_string(),
            password_confirmation: obj.get("passwordConfirmation")?.as_str()?.to_string(),
        })
    }
}
```

And the export, beside `alexandria_auth_local_login`:

```rust
/// Register the local account (UC-41 / FR-AU-10, FR-AU-11): create the
/// single owner's credentials and open a session. `json_body` is the JSON
/// body HTTP would send (`email`, `password`, `passwordConfirmation`). On
/// success `json` carries the `LocalRegisterResult`, whose `sessionId` the
/// caller presents on subsequent requests.
///
/// Deliberately takes no `token`: there is nothing to authenticate with
/// before an account exists. Succeeds only once — a second call returns
/// `AUTH_ERR_CONFLICT` (AF-02).
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_auth_local_register(json_body: *const c_char) -> AuthJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return AuthJsonResult::err(AUTH_ERR_NOT_INITIALIZED),
    };

    let body_str = match cstr_lossy(json_body) {
        Some(s) => s,
        None => return AuthJsonResult::err(AUTH_ERR_INVALID_INPUT),
    };
    let body = match LocalRegisterBody::from_json_str(&body_str) {
        Some(b) => b,
        None => return AuthJsonResult::err(AUTH_ERR_INVALID_INPUT),
    };

    let result = runtime().block_on(async {
        services
            .register_local_account_handler
            .register(body.email, body.password, body.password_confirmation)
            .await
    });

    match result {
        Ok(registration) => {
            let json = serde_json::to_string(&registration).unwrap_or_default();
            AuthJsonResult::ok(json)
        }
        Err(err) => map_auth_err(err),
    }
}
```

- [ ] **Step 5: Regenerate the C header and verify it**

The header is git-ignored, so this is a verification step, not a commit. It proves cbindgen actually exports the new symbol to C callers — a `#[no_mangle]` function that cbindgen skips is invisible to Flutter.

```bash
cargo build -p alexandria-ffi
grep -c "AUTH_ERR_CONFLICT\|alexandria_auth_local_register" crates/alexandria-ffi/src/header.h
```

Expected: a count of at least 2. If it is 0, run `cargo clean -p alexandria-ffi && cargo build -p alexandria-ffi` and check again. Do not edit the header by hand, and do not `git add` it.

- [ ] **Step 6: Run the parity test to verify it passes**

```bash
cargo test -p alexandria-ffi --test parity uc41
```

Expected: PASS.

- [ ] **Step 7: Format, lint, and run the whole suite**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test
```

Expected: green.

- [ ] **Step 8: Commit**

```bash
git add crates/alexandria-ffi/src/lib.rs crates/alexandria-ffi/tests/parity.rs
git commit -m "feat: expose UC-41 registration over FFI"
```

---

### Task 6: UC-35 becomes change-only

The breaking change. `set_credentials` stops being a bootstrap path and always authenticates, and the strength policy applies to it too. Every test that bootstrapped an account through `/credentials` moves to `/register`, and every test password shorter than 12 characters is replaced.

**Files:**
- Modify: `crates/alexandria-core/src/auth/commands/set_credentials.rs`
- Modify: `crates/alexandria-core/tests/auth/set_credentials.rs`
- Modify: `crates/alexandria-http/tests/auth_api.rs`
- Modify: `crates/alexandria-ffi/tests/parity.rs`
- Modify: `crates/alexandria-ffi/src/lib.rs` (doc comment only)

**Interfaces:**
- Consumes: `validate_strength` (Task 1), `POST /v1/auth/local/register` (Task 4), `alexandria_auth_local_register` (Task 5).
- Produces: no new API. `SetLocalCredentialsHandler::set` keeps its signature; only its authorization and validation change.

- [ ] **Step 1: Update the core unit tests to the new contract**

In `crates/alexandria-core/tests/auth/set_credentials.rs`:

Replace the test named `given_first_time_setup_when_set_then_succeeds_without_authenticating` entirely with:

```rust
#[tokio::test]
async fn given_no_credentials_and_no_authentication_when_set_then_unauthorized() {
    // UC-35 is change-only since UC-41: bootstrap goes through
    // registration, so this handler authenticates unconditionally — like
    // every other handler in the codebase.
    let repo = FakeLocalCredentialRepository::new();
    let h = handler(FakeAuth::Denying, repo.clone(), AuthMode::Local);

    let err = h
        .set(
            "owner@example.com".to_string(),
            "correct horse battery".to_string(),
            "",
        )
        .await
        .expect_err("must not bootstrap");

    assert!(matches!(err, DomainError::Unauthorized), "got {err:?}");
    assert!(
        repo.get().await.unwrap().is_none(),
        "nothing may be written on a denied change"
    );
}

#[tokio::test]
async fn given_a_weak_password_when_set_then_invalid_input() {
    // FR-AU-11 applies to changing a password, not only to registering.
    let repo = FakeLocalCredentialRepository::new();
    let h = handler(FakeAuth::Allowing, repo, AuthMode::Local);

    let err = h
        .set("owner@example.com".to_string(), "short".to_string(), "token")
        .await
        .expect_err("must reject a weak password");

    assert!(matches!(err, DomainError::InvalidInput(_)), "got {err:?}");
}
```

Then, throughout the rest of the file, replace every remaining occurrence of the password `"hunter2"` (and any other password shorter than 12 characters) with `"correct horse battery"`, and any *second*, different password with `"another good passphrase"`. Both satisfy the strength policy. Any remaining test that relied on succeeding with `FakeAuth::Denying` must be switched to `FakeAuth::Allowing` with a token, since unauthenticated is now always a rejection.

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test -p alexandria-core --test auth set_credentials
```

Expected: FAIL — the two new tests fail (the handler still bootstraps and still accepts `"short"`).

- [ ] **Step 3: Make the handler change-only**

In `crates/alexandria-core/src/auth/commands/set_credentials.rs`, replace this block inside `set`:

```rust
        // AF-03: unauthenticated is only acceptable when no credentials
        // exist yet (first-time setup).
        if self.repo.get().await?.is_some() {
            self.auth.authenticate(token).await?;
        }

        // AF-02: the email must be well-formed.
        let email = validate_email(&email)?;
        if password.is_empty() {
            return Err(DomainError::InvalidInput("password is required".into()));
        }
```

with:

```rust
        // AF-03: the caller must be authenticated as the owner. Creating
        // the account is UC-41's job, so there is no bootstrap case left.
        self.auth.authenticate(token).await?;

        // AF-02: the email must be well-formed and the password strong.
        let email = validate_email(&email)?;
        validate_strength(&password, &email)?;
```

Add `validate_strength` to the `crate::auth::password` import at the top of the file.

Then replace the handler's doc-comment paragraph that begins "Authorization is conditional, unlike every other handler in this codebase…" (through the end of that paragraph) with:

```rust
/// The caller must be authenticated as the owner: this changes existing
/// credentials, never creates them. Creating the account is UC-41
/// (`RegisterLocalAccountHandler`), which is why the conditional
/// bootstrap branch this handler used to carry is gone — every handler in
/// this codebase now authenticates unconditionally (FR-AU-07).
```

- [ ] **Step 4: Run the core tests to verify they pass**

```bash
cargo test -p alexandria-core --test auth
```

Expected: PASS — register, login, set_credentials, password, and external.

- [ ] **Step 5: Update the HTTP tests**

In `crates/alexandria-http/tests/auth_api.rs`:

Add a helper beside `credentials_request`:

```rust
/// Create the account the way a client now does (UC-41), so the UC-35
/// tests below have credentials to change.
fn register_request(email: &str, password: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/auth/local/register")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "email": email,
                "password": password,
                "passwordConfirmation": password,
            })
            .to_string(),
        ))
        .unwrap()
}
```

Then, in every test that bootstraps by posting to `/v1/auth/local/credentials` with `token: None` and asserting `StatusCode::OK`, replace that bootstrap call with `register_request(…)` asserting `StatusCode::CREATED`. Replace `given_no_credentials_yet_when_set_posted_unauthenticated_then_200_bootstrap_succeeds` with:

```rust
#[tokio::test]
async fn given_no_credentials_yet_when_set_posted_unauthenticated_then_401() {
    // UC-35 is change-only since UC-41; bootstrap is `/register`.
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(credentials_request(
            json!({ "email": "owner@example.com", "password": "correct horse battery" }),
            None,
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
```

Replace every `"hunter2"` in the file with `"correct horse battery"`, and every second/changed password with `"another good passphrase"`. Note the login tests in this file log in with whatever password registration set — keep the two in sync.

- [ ] **Step 6: Update the FFI parity test and doc comment**

In `crates/alexandria-ffi/tests/parity.rs`, the existing UC-34/UC-35 parity test bootstraps on both surfaces through set-credentials with an empty token. On both legs, replace that bootstrap with the registration call (`POST /v1/auth/local/register` for HTTP, `alexandria_auth_local_register` for FFI) using `"correct horse battery"`, then keep the rest of the test — the login comparison — as it is, logging in with that same password. The `assert_eq!(http_set_body, ffi_set_body, …)` line compares registration bodies now: keep the `success` and `email` comparisons and drop the whole-body equality, since `sessionId` differs per surface.

In `crates/alexandria-ffi/src/lib.rs`, update the `alexandria_auth_local_set_credentials` doc comment — the sentence saying `token` is "required only once credentials already exist (AF-03) — pass an empty string on first-time setup" is now false:

```rust
/// Set or change local-login credentials (UC-35 / FR-AU-05, FR-AU-06).
/// `json_body` is the JSON body HTTP would send (`email`, `password`).
/// `token` is required: this changes existing credentials. Creating the
/// account is `alexandria_auth_local_register` (UC-41).
```

Then regenerate the header so the C doc comment matches (verification only — it is git-ignored and never committed):

```bash
cargo build -p alexandria-ffi
```

- [ ] **Step 7: Run the whole suite**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test
```

Expected: green. Any remaining failure is almost certainly a test still using a password below 12 characters, or still bootstrapping through `/credentials` — `grep -rn "hunter2" crates` to find leftovers.

- [ ] **Step 8: Commit**

```bash
git add crates/alexandria-core/src/auth/commands/set_credentials.rs crates/alexandria-core/tests/auth/set_credentials.rs crates/alexandria-http/tests/auth_api.rs crates/alexandria-ffi/tests/parity.rs crates/alexandria-ffi/src/lib.rs
git commit -m "refactor!: make UC-35 change-only, registration is UC-41"
```

---

### Task 7: Documentation

This repo's specs are normative — a use case that is not in the Use Case Specification does not exist. Every document below is updated to match what Tasks 1–6 built.

**Files:**
- Modify: `docs/requirements/Use Case Specification Document.md`
- Modify: `docs/requirements/System Requirements Document.md`
- Modify: `README.md`

**Interfaces:**
- Consumes: everything above. Produces no code.

- [ ] **Step 1: Add UC-41 to the Use Case Specification**

In `docs/requirements/Use Case Specification Document.md`, add a new `### UC-41: Register the local account` section after UC-40, copying the use-case table, Main Flow, and Alternative Flows verbatim from the "UC-41 — Register the local account" section of the spec at `docs/superpowers/specs/2026-08-13-local-account-registration-design.md`. Match the surrounding sections' formatting exactly (the `| Field | Value |` table, the numbered Main Flow, the `| ID | Condition | Outcome |` Alternative Flows table, and a `---` separator).

Then add a row to the traceability table at the end of the file, after the UC-40 row:

```markdown
| UC-41: Register the local account | FR-AU-05, FR-AU-06, FR-AU-08, FR-AU-09, FR-AU-10, FR-AU-11 |
```

- [ ] **Step 2: Amend UC-35 in the same document**

In the UC-35 section:

- Change the **Preconditions** cell to: `The active auth mode is local login; the caller is authenticated as the owner; local credentials already exist.`
- Change the **Requirements** cell to: `FR-AU-05, FR-AU-06, FR-AU-07, FR-AU-08, FR-AU-11`
- Replace the AF-03 row with: `| AF-03 | The caller is not authenticated | The system denies with an unauthorized error. Creating the account is UC-41, not this use case. |`
- Add an AF-04 row: `| AF-04 | The password does not satisfy the strength policy | The system rejects with an invalid-input error naming the unmet rule; no plaintext is logged. |`
- Update the UC-35 traceability row to the same requirement list as above.

- [ ] **Step 3: Update the System Requirements Document**

In `docs/requirements/System Requirements Document.md`:

Add two rows after FR-AU-09 in the functional requirements table:

```markdown
| FR-AU-10 | In local mode, the system shall provide a registration operation that creates the single owner's credential row when none exists, opens a session for the caller, and rejects any subsequent registration as a conflict. |
| FR-AU-11 | The system shall reject a local password that is shorter than 12 characters, longer than 128 characters, entirely whitespace, a single repeated character, equal to or containing the submitted email address, or one of a list of common passwords. |
```

In the HTTP endpoint table, add a row above the `/v1/auth/local/login` row:

```markdown
| POST | /v1/auth/local/register | Create the owner's local account and open a session (local mode). | FR-AU-10, FR-AU-11 |
```

and change the `/v1/auth/local/credentials` row's description to `Change existing local credentials (local mode).`

Around line 326, the credential-row entity note describes the row as created by the setup operation — update it to say the row is created by UC-41 registration and changed by UC-35. Update the F-09 coverage row (`| F-09 Pluggable authentication | FR-AU-01 through FR-AU-09 |`) to read `FR-AU-01 through FR-AU-11`.

- [ ] **Step 4: Update the README**

In `README.md`:

Change the F-09 milestone row's count from `3 / 3` to `3 / 4`, and its use case range from `UC-34 … UC-36` to `UC-34 … UC-36, UC-41`.

Add a row to the F-09 backlog table (leave the issue link as the issue number this work is tracked under, and the checkbox unchecked until the PR merges):

```markdown
| [#TBD](https://github.com/artur-rios/alexandria-api/issues/TBD) | UC-41 | &#9744; | Register the local account | FR-AU-10, FR-AU-11 |
```

Replace `#TBD` with the real issue number before committing — if no issue exists yet, create one titled "UC-41: Register the local account" and use its number.

In the auth prose around line 436, replace the sentence "set the owner's credentials once via the local credential setup operation (UC-35) before callers can authenticate" with:

```markdown
create the owner's account once via `POST /v1/auth/local/register` (UC-41) —
it takes `email`, `password`, and `passwordConfirmation`, succeeds only once,
and returns a `sessionId` you are immediately authenticated with. Passwords
must be at least 12 characters. `POST /v1/auth/local/credentials` (UC-35)
changes those credentials afterwards and requires an authenticated session.
```

- [ ] **Step 5: Verify the docs match the code**

```bash
grep -rn "UC-41" docs/requirements README.md
```

Expected: the new use case section, both traceability rows, the endpoint row, the README milestone and backlog rows, and the auth prose. Confirm no document still tells a reader to bootstrap through UC-35.

- [ ] **Step 6: Commit**

```bash
git add docs/requirements README.md
git commit -m "docs: specify UC-41 and amend UC-35 to change-only"
```

---

### Task 8: Open the pull request

**Files:** none.

- [ ] **Step 1: Run the full suite one last time from a clean build**

```bash
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test
```

Expected: green, with no formatting diff. Do not proceed on a failure.

- [ ] **Step 2: Confirm no build artifact was accidentally staged**

```bash
git ls-files crates/alexandria-ffi/src/header.h
```

Expected: empty output. The generated header is git-ignored; any output here means it was force-added and must be removed with `git rm --cached crates/alexandria-ffi/src/header.h`.

- [ ] **Step 3: Push and open the PR**

```bash
git push -u origin feature/uc-41-register-local-account
```

Then open a pull request into `main` whose description references the use case and its issue (`Closes #<issue-number>`), summarizes the new endpoint, and calls out the breaking change to `POST /v1/auth/local/credentials` explicitly.

- [ ] **Step 4: Stop**

Hand off to a human for review. Do not self-approve and do not merge (Development Workflow §5 Step 7).

---

## Notes for the implementer

- **`validate_email` lives in `set_credentials.rs`**, not in a shared module, and Task 3 imports it from there. That is deliberate: moving it would touch a file this feature otherwise only edits surgically, and both callers are auth commands. If a third caller ever appears, that is the moment to promote it.
- **AF-01 is not reachable from the HTTP or FFI integration tests.** The active auth mode is fixed at startup from `Settings`, and both test harnesses force `AuthMode::Local`. It is covered by a core unit test instead — the same choice the existing UC-34/UC-35 tests already made, and their module doc comments say so.
- **Do not add a `passwordConfirmation` field to UC-35.** By the time a caller changes a password they hold a valid session, so a typo is recoverable through it. Registration is the only irrecoverable moment.
- **Registration is ungated but not unguarded.** If you find yourself wanting to put `/register` behind `require_auth`, re-read UC-41 AF-02: the endpoint is safe precisely because it succeeds exactly once.
