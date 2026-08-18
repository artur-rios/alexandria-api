# Local Recovery Codes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace local mode's e-mail confirmation and password reset with ten single-use recovery codes issued at registration, redeemed together with a new password.

**Architecture:** A pure `recovery.rs` primitive (generate, normalize, hash) beside the existing `password.rs`, a `RecoveryCodeRepository` port beside `SessionRepository`, and two new command handlers. Added first, wired second, and only then is the e-mail machinery removed — so every task boundary compiles and passes.

**Tech Stack:** Rust 2021, `sqlx` (SQLite), `argon2`'s re-exported `OsRng`, `sha2` via the existing `catalog::fs::sha256_hex`, `axum` 0.8, `serde`.

## Global Constraints

- **Design spec:** [`docs/superpowers/specs/2026-08-18-local-recovery-codes-design.md`](../specs/2026-08-18-local-recovery-codes-design.md). Every decision traces to it; do not improvise past it.
- **Ten codes** (`RECOVERY_CODE_COUNT = 10`), **ten characters each** (`RECOVERY_CODE_LENGTH = 10`), Crockford base32 — `0123456789ABCDEFGHJKMNPQRSTVWXYZ`, no `I`/`L`/`O`/`U` — rendered `XXXXX-XXXXX`.
- **Only hashes are stored.** SHA-256 of the normalized code, never the plaintext, never Argon2.
- **A failed redemption consumes nothing.** The password policy is checked before the code table is touched.
- **Reason codes:** `recovery_code_unknown` and `recovery_code_used` are distinct (FR-AU-15).
- **Both surfaces carry every operation** (FR-AU-08): each new HTTP route gets an FFI export and a parity test.
- Test naming: `given_<condition>_when_<action>_then_<outcome>`.
- Run from the repo root. `cargo test --workspace` links FFmpeg and is slow on a cold cache — expected.
- `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --check` must be clean before every commit.
- Commit subjects: lowercase Conventional Commits, ≤50 chars, imperative.

---

### Task 1: The recovery code primitive

Pure functions only — no repository, no clock, no database. Mirrors how
`auth/password.rs` owns the password's representation.

**Files:**
- Create: `crates/alexandria-core/src/auth/recovery.rs`
- Modify: `crates/alexandria-core/src/auth/mod.rs`

**Interfaces:**
- Consumes: `crate::catalog::fs::sha256_hex(bytes: &[u8]) -> String` (exists).
- Produces:
  - `pub const RECOVERY_CODE_COUNT: usize = 10`
  - `pub const RECOVERY_CODE_LENGTH: usize = 10`
  - `pub fn generate_recovery_codes() -> Vec<String>` — `RECOVERY_CODE_COUNT` formatted codes
  - `pub fn normalize_recovery_code(input: &str) -> String`
  - `pub fn hash_recovery_code(input: &str) -> String`

- [ ] **Step 1: Declare the module**

In `crates/alexandria-core/src/auth/mod.rs`, add `pub mod recovery;` to the
module list, keeping it alphabetical (after `pub mod password;`).

- [ ] **Step 2: Write the failing tests**

Create `crates/alexandria-core/src/auth/recovery.rs` containing **only** this
test module for now:

```rust
#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn given_generation_when_called_then_ten_distinct_codes() {
        let codes = generate_recovery_codes();

        assert_eq!(codes.len(), RECOVERY_CODE_COUNT);
        let distinct: HashSet<&String> = codes.iter().collect();
        assert_eq!(distinct.len(), RECOVERY_CODE_COUNT, "codes repeated: {codes:?}");
    }

    /// The alphabet excludes I, L, O and U so nothing on a printed list is
    /// ambiguous, and the hyphen is presentation only.
    #[test]
    fn given_a_generated_code_when_inspected_then_it_is_two_groups_of_five_crockford_base32() {
        for code in generate_recovery_codes() {
            let (left, right) = code.split_once('-').expect("no hyphen in {code}");
            assert_eq!(left.len(), 5, "{code}");
            assert_eq!(right.len(), 5, "{code}");
            for character in code.chars().filter(|c| *c != '-') {
                assert!(
                    RECOVERY_CODE_ALPHABET.contains(&(character as u8)),
                    "{character} in {code} is not in the alphabet"
                );
            }
        }
    }

    /// A code is typed off paper, so the hyphen, the case, and stray spaces
    /// must not decide whether it works.
    #[test]
    fn given_the_same_code_written_four_ways_when_normalized_then_all_agree() {
        let spellings = ["ABCDE-FGHJK", "abcde-fghjk", "ABCDEFGHJK", "  abcde fghjk  "];

        let normalized: HashSet<String> =
            spellings.iter().map(|s| normalize_recovery_code(s)).collect();

        assert_eq!(normalized.len(), 1, "{normalized:?}");
        assert_eq!(normalized.into_iter().next().unwrap(), "ABCDEFGHJK");
    }

    #[test]
    fn given_the_same_code_written_four_ways_when_hashed_then_all_agree() {
        let hashes: HashSet<String> = ["ABCDE-FGHJK", "abcde-fghjk", "ABCDEFGHJK", " abcde fghjk "]
            .iter()
            .map(|s| hash_recovery_code(s))
            .collect();

        assert_eq!(hashes.len(), 1);
    }

    #[test]
    fn given_two_different_codes_when_hashed_then_hashes_differ() {
        assert_ne!(hash_recovery_code("ABCDE-FGHJK"), hash_recovery_code("ABCDE-FGHJM"));
    }

    /// The stored value must not be reversible to the code, and must not
    /// simply *be* the code.
    #[test]
    fn given_a_code_when_hashed_then_the_hash_is_sha256_hex_and_not_the_code() {
        let hash = hash_recovery_code("ABCDE-FGHJK");

        assert_eq!(hash.len(), 64, "{hash}");
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!hash.contains("ABCDE"));
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p alexandria-core recovery`
Expected: FAIL to compile — `cannot find function generate_recovery_codes in this scope`.

- [ ] **Step 4: Write the implementation**

Prepend to `crates/alexandria-core/src/auth/recovery.rs`, above the test module:

```rust
//! Recovery codes: the only way back into a local account whose password has
//! been forgotten (FR-AU-13 … FR-AU-19).
//!
//! Pure functions over one credential's representation, in the same shape as
//! `password.rs` — no repository, no clock — so the format and the hash are
//! testable without a database.

use argon2::password_hash::rand_core::{OsRng, RngCore};

use crate::catalog::fs::sha256_hex;

/// How many codes an owner holds at once. Ten, because a recovery method that
/// runs out on the second mistake is not one.
pub const RECOVERY_CODE_COUNT: usize = 10;

/// Characters per code, excluding the presentational hyphen. Ten characters
/// of a 32-symbol alphabet is fifty bits — far past guessing, and still short
/// enough to write on paper.
pub const RECOVERY_CODE_LENGTH: usize = 10;

/// Crockford base32: the digits and upper-case letters minus `I`, `L`, `O`
/// and `U`, so no character on a printed list can be mistaken for another.
/// Exactly 32 symbols, so masking a random byte to five bits is uniform —
/// 256 divides by 32, leaving no modulo bias to correct for.
const RECOVERY_CODE_ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// How many characters precede the hyphen when a code is displayed.
const RECOVERY_CODE_GROUP: usize = 5;

/// Generate a fresh set of `RECOVERY_CODE_COUNT` codes, formatted for display.
///
/// Distinctness is not enforced by a retry loop: at fifty bits each, a
/// collision within ten draws is far less likely than the disk failing
/// mid-write, and a loop would add a branch nothing can ever exercise.
pub fn generate_recovery_codes() -> Vec<String> {
    (0..RECOVERY_CODE_COUNT).map(|_| generate_one()).collect()
}

fn generate_one() -> String {
    let mut bytes = [0u8; RECOVERY_CODE_LENGTH];
    OsRng.fill_bytes(&mut bytes);

    let mut code = String::with_capacity(RECOVERY_CODE_LENGTH + 1);
    for (index, byte) in bytes.iter().enumerate() {
        if index == RECOVERY_CODE_GROUP {
            code.push('-');
        }
        code.push(RECOVERY_CODE_ALPHABET[usize::from(byte & 0x1f)] as char);
    }
    code
}

/// Reduce a code as typed to the canonical form that was hashed.
///
/// Upper-cases and drops everything that is not a letter or a digit, so the
/// hyphen a client printed, the case the owner used, and any spaces they
/// typed cannot decide whether their code works.
pub fn normalize_recovery_code(input: &str) -> String {
    input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// Hash a code the way it is stored: normalized, then SHA-256.
///
/// SHA-256 rather than Argon2 for the reason `tokens.rs` gave before it:
/// Argon2's work factor exists to slow the guessing of a secret a person
/// chose, and this is fifty bits chosen by the operating system. The search
/// space already does that job; a slow hash would only be a cost paid on
/// every lookup. Storing the hash rather than the code still matters, and for
/// FR-AU-06's reason — a database read must not yield a working credential.
pub fn hash_recovery_code(input: &str) -> String {
    sha256_hex(normalize_recovery_code(input).as_bytes())
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p alexandria-core recovery`
Expected: PASS, 6 tests.

- [ ] **Step 6: Check lints and formatting**

Run: `cargo clippy -p alexandria-core --all-targets -- -D warnings && cargo fmt --check`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add crates/alexandria-core/src/auth/recovery.rs crates/alexandria-core/src/auth/mod.rs
git commit -m "feat: add the recovery code primitive"
```

---

### Task 2: Storage — migration and repository

Creates the table and its port. This migration only *creates*; the drop of the
e-mail tables waits for Task 5, after the code that reads them is gone.

**Files:**
- Create: `crates/alexandria-core/migrations/00000000000013_recovery_codes.sql`
- Modify: `crates/alexandria-core/src/auth/local.rs`
- Test: `crates/alexandria-core/tests/auth/recovery_repo.rs` (create), `crates/alexandria-core/tests/auth.rs` (declare the module)

**Test harness, so you do not have to discover it:** handler and repository
tests in this crate are *integration* tests under `crates/alexandria-core/tests/`,
not `#[cfg(test)]` modules in the source files — no command source file has
one. `tests/auth.rs` is the binary; each test file is pulled in with an
explicit path attribute, e.g.

```rust
#[path = "auth/recovery_repo.rs"]
mod recovery_repo;
```

Shared fakes (`FakeLocalCredentialRepository`, `FakeSessionRepository`,
`FixedClock`, …) live in `crates/alexandria-core/tests/common/mod.rs`. A
migrated pool comes from `alexandria_core::migrate::migrate_database(path)`
against a `tempfile` directory — see `crates/alexandria-core/tests/playback.rs`
for the two-line version.

**Interfaces:**
- Consumes: `hash_recovery_code` (Task 1).
- Produces, all in `crate::auth::local`:
  - `pub enum RecoveryCodeOutcome { Consumed, AlreadyUsed, Unknown }` (derives `Debug, Clone, Copy, PartialEq, Eq`)
  - `pub trait RecoveryCodeRepository: Send + Sync` with
    `async fn replace_all(&self, code_hashes: &[String], created_at: DateTime<Utc>) -> Result<(), DomainError>`,
    `async fn consume(&self, code_hash: &str, now: DateTime<Utc>) -> Result<RecoveryCodeOutcome, DomainError>`,
    `async fn remaining(&self) -> Result<u32, DomainError>`
  - `pub struct SqliteRecoveryCodeRepository` with `pub fn new(pool: SqlitePool) -> Self`, deriving `Clone`

- [ ] **Step 1: Write the migration**

Create `crates/alexandria-core/migrations/00000000000013_recovery_codes.sql`:

```sql
-- UC-43/UC-44: the recovery codes that replace e-mail password reset
-- (FR-AU-13 … FR-AU-19).
--
-- Only `code_hash` is stored, never the code itself. A recovery code
-- overrides the password, so a database read that yielded a working code
-- would be exactly as bad as one that yielded a working password — the same
-- reasoning that keeps FR-AU-06 from storing a plaintext one. Lookups hash
-- the presented value and match on that, which is why the hash carries the
-- unique index.
--
-- There is no expiry column. A code is written on paper and used years later
-- or never; an expiry would silently turn the owner's only way back in into
-- nothing, on a schedule they did not choose. `consumed_at` is the whole
-- lifecycle: NULL means usable, set means spent.
CREATE TABLE IF NOT EXISTS recovery_codes (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    code_hash   TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    consumed_at TEXT
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_recovery_codes_hash
    ON recovery_codes (code_hash);
```

- [ ] **Step 2: Write the failing tests**

Create `crates/alexandria-core/tests/auth/recovery_repo.rs` and add
`#[path = "auth/recovery_repo.rs"] mod recovery_repo;` to `tests/auth.rs`.

This is the one file in this plan that needs a real database — it is testing
SQL. Everything else uses fakes.

```rust
use alexandria_core::auth::local::{
    RecoveryCodeOutcome, RecoveryCodeRepository, SqliteRecoveryCodeRepository,
};
use alexandria_core::auth::recovery::{generate_recovery_codes, hash_recovery_code};
use alexandria_core::migrate::migrate_database;
use chrono::Utc;

/// A migrated database in a fresh temporary directory, the way
/// `tests/playback.rs` does it. The `TempDir` is returned so the caller holds
/// it: dropping it deletes the database out from under the pool.
async fn repo() -> (SqliteRecoveryCodeRepository, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("recovery.sqlite");
    let pool = migrate_database(path.to_str().expect("utf-8 path"))
        .await
        .expect("migrate");
    (SqliteRecoveryCodeRepository::new(pool), dir)
}

#[tokio::test]
async fn given_stored_codes_when_remaining_then_counts_the_unconsumed() {
    let (repo, _dir) = repo().await;
    let codes = generate_recovery_codes();
    let hashes: Vec<String> = codes.iter().map(|c| hash_recovery_code(c)).collect();

    repo.replace_all(&hashes, Utc::now()).await.unwrap();

    assert_eq!(repo.remaining().await.unwrap(), 10);
}

#[tokio::test]
async fn given_an_unconsumed_code_when_consumed_then_consumed_and_the_count_drops() {
    let (repo, _dir) = repo().await;
    let codes = generate_recovery_codes();
    let hashes: Vec<String> = codes.iter().map(|c| hash_recovery_code(c)).collect();
    repo.replace_all(&hashes, Utc::now()).await.unwrap();

    let outcome = repo.consume(&hashes[3], Utc::now()).await.unwrap();

    assert_eq!(outcome, RecoveryCodeOutcome::Consumed);
    assert_eq!(repo.remaining().await.unwrap(), 9);
}

#[tokio::test]
async fn given_an_already_consumed_code_when_consumed_again_then_already_used() {
    let (repo, _dir) = repo().await;
    let codes = generate_recovery_codes();
    let hashes: Vec<String> = codes.iter().map(|c| hash_recovery_code(c)).collect();
    repo.replace_all(&hashes, Utc::now()).await.unwrap();
    repo.consume(&hashes[0], Utc::now()).await.unwrap();

    let outcome = repo.consume(&hashes[0], Utc::now()).await.unwrap();

    assert_eq!(outcome, RecoveryCodeOutcome::AlreadyUsed);
    assert_eq!(repo.remaining().await.unwrap(), 9);
}

#[tokio::test]
async fn given_a_hash_that_was_never_stored_when_consumed_then_unknown() {
    let (repo, _dir) = repo().await;
    repo.replace_all(&[hash_recovery_code("ABCDE-FGHJK")], Utc::now())
        .await
        .unwrap();

    let outcome = repo
        .consume(&hash_recovery_code("MNPQR-STVWX"), Utc::now())
        .await
        .unwrap();

    assert_eq!(outcome, RecoveryCodeOutcome::Unknown);
}

/// Regeneration must invalidate the codes the owner still holds, not just the
/// spent ones — a partial refill would leave them unsure which of their
/// written codes still work (FR-AU-17).
#[tokio::test]
async fn given_existing_codes_when_replaced_then_every_old_code_is_gone_including_unused() {
    let (repo, _dir) = repo().await;
    let first: Vec<String> = generate_recovery_codes()
        .iter()
        .map(|c| hash_recovery_code(c))
        .collect();
    repo.replace_all(&first, Utc::now()).await.unwrap();
    repo.consume(&first[0], Utc::now()).await.unwrap();

    let second: Vec<String> = generate_recovery_codes()
        .iter()
        .map(|c| hash_recovery_code(c))
        .collect();
    repo.replace_all(&second, Utc::now()).await.unwrap();

    assert_eq!(repo.remaining().await.unwrap(), 10);
    assert_eq!(
        repo.consume(&first[5], Utc::now()).await.unwrap(),
        RecoveryCodeOutcome::Unknown,
        "an unused code from the previous set survived regeneration"
    );
}

#[tokio::test]
async fn given_no_codes_when_remaining_then_zero() {
    let (repo, _dir) = repo().await;
    assert_eq!(repo.remaining().await.unwrap(), 0);
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p alexandria-core --test auth recovery_repo`
Expected: FAIL to compile — `RecoveryCodeRepository` and
`SqliteRecoveryCodeRepository` do not exist.

- [ ] **Step 4: Write the port and its implementation**

Append to `crates/alexandria-core/src/auth/local.rs`, beside the existing
session repository (imports `DateTime`, `Utc`, `SqlitePool`, `Row` are already
in the file):

```rust
/// What `RecoveryCodeRepository::consume` found (FR-AU-15).
///
/// Three states rather than a boolean, because an owner working down a
/// printed list needs to know whether they mistyped a code or already spent
/// it, and only storage can tell those apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryCodeOutcome {
    /// The code existed, was unused, and is now spent.
    Consumed,
    /// The code exists but was already redeemed.
    AlreadyUsed,
    /// No such code was ever issued — or it belonged to a set that has since
    /// been regenerated away.
    Unknown,
}

/// Recovery code storage port (UC-43/UC-44). Unit-testable against an
/// in-memory fake with no database (Testing Specification §6.2).
#[allow(async_fn_in_trait)]
pub trait RecoveryCodeRepository: Send + Sync {
    /// Delete every existing code and store this set.
    ///
    /// One method serves registration and regeneration both, because "the
    /// owner's codes are exactly these ten" is one idea and splitting it
    /// would let the two paths drift.
    async fn replace_all(
        &self,
        code_hashes: &[String],
        created_at: DateTime<Utc>,
    ) -> Result<(), DomainError>;

    /// Spend the code with this hash, reporting what was found.
    async fn consume(
        &self,
        code_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<RecoveryCodeOutcome, DomainError>;

    /// How many codes remain unconsumed (FR-AU-18).
    async fn remaining(&self) -> Result<u32, DomainError>;
}

#[derive(Clone)]
pub struct SqliteRecoveryCodeRepository {
    pool: SqlitePool,
}

impl SqliteRecoveryCodeRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

impl RecoveryCodeRepository for SqliteRecoveryCodeRepository {
    async fn replace_all(
        &self,
        code_hashes: &[String],
        created_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        let created_at = created_at.to_rfc3339();
        let mut tx = self.pool.begin().await?;

        // One transaction, unlike the cross-port writes elsewhere in this
        // module: both statements belong to this single repository, so there
        // is no second port to share it with, and a delete that committed
        // without its insert would leave the owner with no codes at all.
        sqlx::query("DELETE FROM recovery_codes")
            .execute(&mut *tx)
            .await?;

        for code_hash in code_hashes {
            sqlx::query(
                "INSERT INTO recovery_codes (code_hash, created_at, consumed_at) \
                 VALUES (?, ?, NULL)",
            )
            .bind(code_hash)
            .bind(&created_at)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    async fn consume(
        &self,
        code_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<RecoveryCodeOutcome, DomainError> {
        // The conditional UPDATE is the whole concurrency story: two
        // simultaneous redemptions of one code cannot both affect a row, so
        // exactly one sees `Consumed`. A read-then-write would let both pass
        // the read.
        let updated = sqlx::query(
            "UPDATE recovery_codes SET consumed_at = ? \
             WHERE code_hash = ? AND consumed_at IS NULL",
        )
        .bind(now.to_rfc3339())
        .bind(code_hash)
        .execute(&self.pool)
        .await?;

        if updated.rows_affected() == 1 {
            return Ok(RecoveryCodeOutcome::Consumed);
        }

        // Nothing was updated: either the code is spent, or it was never
        // issued. Only this second read can say which.
        let exists = sqlx::query("SELECT 1 FROM recovery_codes WHERE code_hash = ?")
            .bind(code_hash)
            .fetch_optional(&self.pool)
            .await?
            .is_some();

        Ok(if exists {
            RecoveryCodeOutcome::AlreadyUsed
        } else {
            RecoveryCodeOutcome::Unknown
        })
    }

    async fn remaining(&self) -> Result<u32, DomainError> {
        let row = sqlx::query("SELECT COUNT(*) AS remaining FROM recovery_codes WHERE consumed_at IS NULL")
            .fetch_one(&self.pool)
            .await?;
        let remaining: i64 = row.try_get("remaining")?;
        Ok(u32::try_from(remaining).unwrap_or(u32::MAX))
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p alexandria-core --test auth recovery_repo`
Expected: PASS, 6 tests.

- [ ] **Step 6: Confirm the migration applies cleanly**

Run: `cargo test -p alexandria-core --test migrations`
Expected: PASS — the migration suite runs every migration in order against a
fresh database.

- [ ] **Step 7: Check lints and formatting**

Run: `cargo clippy -p alexandria-core --all-targets -- -D warnings && cargo fmt --check`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add crates/alexandria-core/migrations crates/alexandria-core/src/auth/local.rs crates/alexandria-core/tests
git commit -m "feat: store recovery codes"
```

---

### Task 3: Registration issues codes, account reports the count

Registration gains the code set; `account` gains the remaining count. The
e-mail fields stay for now — Task 5 removes them, so this task's diff is purely
additive and can be reviewed on its own.

**Files:**
- Modify: `crates/alexandria-core/src/auth/local.rs` (the two result structs)
- Modify: `crates/alexandria-core/src/auth/commands/register.rs`
- Modify: `crates/alexandria-core/src/auth/commands/account_status.rs`
- Modify: `crates/alexandria-core/src/services.rs` (wire the new repository)
- Modify: `crates/alexandria-core/tests/common/mod.rs` (add the fake)
- Modify: `crates/alexandria-core/tests/auth/register.rs`
- Create: `crates/alexandria-core/tests/auth/account_status.rs`, declared in `tests/auth.rs` as `#[path = "auth/account_status.rs"] mod account_status;`

**Interfaces:**
- Consumes: `generate_recovery_codes`, `hash_recovery_code` (Task 1); `RecoveryCodeRepository`, `SqliteRecoveryCodeRepository` (Task 2).
- Produces:
  - `LocalRegisterResult` gains `pub recovery_codes: Vec<String>`
  - `LocalAccountResult` gains `pub recovery_codes_remaining: u32`
  - `RegisterLocalAccountHandler::new` gains a `recovery_codes: RR` parameter, appended after `sessions`
  - `GetLocalAccountHandler::new` gains a `recovery_codes: RR` parameter, appended after `credentials`

- [ ] **Step 1: Write the failing tests**

First add a `FakeRecoveryCodeRepository` to
`crates/alexandria-core/tests/common/mod.rs`, in the style of the
`FakeSessionRepository` already there: a `Clone` struct wrapping
`Arc<Mutex<Vec<(String, Option<DateTime<Utc>>)>>>`, implementing
`RecoveryCodeRepository` over that vector, plus a `stored_hashes(&self) ->
Vec<String>` accessor the tests below use. Its `consume` must return the same
three `RecoveryCodeOutcome` states the SQLite one does, or the handler tests
will not exercise the real branches.

Then add these cases to `crates/alexandria-core/tests/auth/register.rs`,
following that file's existing `handler(...)` helper — extend the helper and
the `TestRegisterHandler` type alias to carry the new repository rather than
writing a second helper:

```rust
    #[tokio::test]
    async fn given_a_first_registration_when_registered_then_ten_distinct_codes_are_returned() {
        let (handler, _credentials, recovery) = handler_with_recovery();

        let result = handler
            .register(
                "owner@example.com".to_string(),
                "correct horse battery".to_string(),
                "correct horse battery".to_string(),
            )
            .await
            .unwrap();

        assert_eq!(result.recovery_codes.len(), 10);
        let distinct: std::collections::HashSet<&String> = result.recovery_codes.iter().collect();
        assert_eq!(distinct.len(), 10);
        assert_eq!(recovery.remaining().await.unwrap(), 10);
    }

    /// The plaintext exists only in the response (FR-AU-19).
    #[tokio::test]
    async fn given_a_registration_when_codes_are_stored_then_no_stored_value_is_a_code() {
        let (handler, _credentials, recovery) = handler_with_recovery();

        let result = handler
            .register(
                "owner@example.com".to_string(),
                "correct horse battery".to_string(),
                "correct horse battery".to_string(),
            )
            .await
            .unwrap();

        let stored = recovery.stored_hashes();
        for code in &result.recovery_codes {
            assert!(!stored.contains(code), "{code} was stored verbatim");
            assert!(
                stored.contains(&hash_recovery_code(code)),
                "the hash of {code} was not stored"
            );
        }
    }

    /// A registration that never happens must leave no codes behind.
    #[tokio::test]
    async fn given_a_rejected_registration_when_attempted_then_no_codes_are_stored() {
        let (handler, _credentials, recovery) = handler_with_recovery();

        let result = handler
            .register(
                "owner@example.com".to_string(),
                "short".to_string(),
                "short".to_string(),
            )
            .await;

        assert!(result.is_err());
        assert_eq!(recovery.remaining().await.unwrap(), 0);
    }
```

And create `crates/alexandria-core/tests/auth/account_status.rs` — there is no
test file for this handler yet, so it needs its own `handler_with_recovery()`
helper built from `FakeLocalCredentialRepository`, a fake auth service that
authenticates, and the new `FakeRecoveryCodeRepository`:

```rust
    #[tokio::test]
    async fn given_codes_remaining_when_account_is_read_then_the_count_is_reported() {
        let (handler, recovery) = handler_with_recovery();
        recovery
            .replace_all(&vec!["a".to_string(), "b".to_string()], Utc::now())
            .await
            .unwrap();

        let result = handler.get("session").await.unwrap();

        assert_eq!(result.recovery_codes_remaining, 2);
    }

    /// An account registered before recovery codes existed holds none, and the
    /// count is how its owner learns to regenerate.
    #[tokio::test]
    async fn given_an_account_with_no_codes_when_read_then_zero_is_reported() {
        let (handler, _recovery) = handler_with_recovery();

        assert_eq!(handler.get("session").await.unwrap().recovery_codes_remaining, 0);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p alexandria-core --test auth register account_status`
Expected: FAIL to compile — `recovery_codes` and `recovery_codes_remaining` are
not fields.

- [ ] **Step 3: Extend the result types**

In `crates/alexandria-core/src/auth/local.rs`, add to `LocalRegisterResult`:

```rust
    /// The owner's recovery codes, in plaintext, returned exactly once
    /// (FR-AU-13). They are not retrievable afterwards: only their hashes are
    /// stored, so this response is the only chance to record them.
    pub recovery_codes: Vec<String>,
```

and to `LocalAccountResult`:

```rust
    /// How many recovery codes are still unspent (FR-AU-18).
    ///
    /// Zero means the account cannot currently be recovered — either every
    /// code has been used, or the account predates them — and the owner
    /// should regenerate while they still know their password.
    pub recovery_codes_remaining: u32,
```

- [ ] **Step 4: Issue codes at registration**

In `register.rs`, add `RR` to the handler's generic parameters and a
`recovery_codes: RR` field bounded `RR: RecoveryCodeRepository`, taking it as
the parameter after `sessions` in `new`. Then, immediately after the
`insert_if_absent` success check and before the session is issued:

```rust
        // FR-AU-13: the codes are the account's only recovery path, so they
        // are written before the caller is told the account exists. Their
        // plaintext is returned here and nowhere else, ever.
        let recovery_codes = generate_recovery_codes();
        let hashes: Vec<String> = recovery_codes.iter().map(|c| hash_recovery_code(c)).collect();
        self.recovery_codes
            .replace_all(&hashes, self.clock.now())
            .await?;
```

and add `recovery_codes` to the returned `LocalRegisterResult`.

Import `use crate::auth::recovery::{generate_recovery_codes, hash_recovery_code};`
and `RecoveryCodeRepository` from `crate::auth::local`.

- [ ] **Step 5: Report the count from account status**

In `account_status.rs`, add `RR` to the generics and a `recovery_codes: RR`
field taken after `credentials` in `new`, then:

```rust
        Ok(LocalAccountResult {
            recovery_codes_remaining: self.recovery_codes.remaining().await?,
            email: credential.email,
            email_confirmed: credential.email_confirmed(),
        })
```

- [ ] **Step 6: Wire the repository at runtime**

In `crates/alexandria-core/src/services.rs`, construct
`SqliteRecoveryCodeRepository::new(pool.clone())` beside the existing session
repository and pass it to both handlers' constructors, updating their type
aliases to carry the new parameter.

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test --workspace`
Expected: PASS. Update the existing HTTP and FFI tests that assert on the
registration response shape — they will need the new field.

- [ ] **Step 8: Check lints and formatting**

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check`
Expected: clean.

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "feat: issue recovery codes at registration"
```

---

### Task 4: Redeem and regenerate

The two new operations, their routes, their FFI exports, and their parity tests.

**Files:**
- Create: `crates/alexandria-core/src/auth/commands/redeem_recovery_code.rs`
- Create: `crates/alexandria-core/src/auth/commands/regenerate_recovery_codes.rs`
- Modify: `crates/alexandria-core/src/auth/commands/mod.rs`, `crates/alexandria-core/src/auth/local.rs` (result types), `crates/alexandria-core/src/services.rs`
- Modify: `crates/alexandria-http/src/routes/auth.rs`, `crates/alexandria-ffi/src/lib.rs`
- Create: `crates/alexandria-core/tests/auth/recovery_redeem.rs`, `crates/alexandria-core/tests/auth/recovery_regenerate.rs`, both declared in `tests/auth.rs` with `#[path = …]` attributes
- Test: parity tests in `crates/alexandria-ffi/tests/parity.rs`

**Where the tests go:** the two handler suites below are integration tests under
`crates/alexandria-core/tests/auth/`, built from the fakes in
`tests/common/mod.rs` — no command source file in this crate carries a
`#[cfg(test)]` module. `crates/alexandria-core/tests/auth/password_reset.rs`
is the closest existing analogue for redemption and
`tests/auth/set_credentials.rs` for regeneration; read both before writing.

**Interfaces:**
- Consumes: `RecoveryCodeRepository`, `RecoveryCodeOutcome` (Task 2); `hash_recovery_code` (Task 1); `LocalCredentialRepository`, `SessionRepository`, `validate_strength`, `hash_password`.
- Produces:
  - `pub struct RedeemRecoveryCodeResult { pub success: bool, pub email: String, pub recovery_codes_remaining: u32 }`
  - `pub struct RegenerateRecoveryCodesResult { pub recovery_codes: Vec<String> }`
  - `RedeemRecoveryCodeHandler::new(credentials, sessions, recovery_codes, clock, mode)`
  - `RegenerateRecoveryCodesHandler::new(auth, credentials, recovery_codes, clock, mode)`
  - `POST /v1/auth/local/recovery/redeem`, `POST /v1/auth/local/recovery/regenerate`
  - `alexandria_auth_local_redeem_recovery_code`, `alexandria_auth_local_regenerate_recovery_codes`

- [ ] **Step 1: Write the failing tests for redemption**

Create `crates/alexandria-core/tests/auth/recovery_redeem.rs` and declare it in
`tests/auth.rs`. Model the helpers on `tests/auth/password_reset.rs`, which
already fakes credentials, sessions and a fixed clock.

```rust
use alexandria_core::auth::commands::redeem_recovery_code::RedeemRecoveryCodeHandler;
use alexandria_core::auth::local::{RecoveryCodeOutcome, RecoveryCodeRepository};
use alexandria_core::config::AuthMode;
use alexandria_core::errors::DomainError;

use crate::common::{
    FakeLocalCredentialRepository, FakeRecoveryCodeRepository, FakeSessionRepository,
};

const NEW_PASSWORD: &str = "correct horse battery";

    #[tokio::test]
    async fn given_a_valid_code_when_redeemed_then_password_replaced_and_sessions_cleared() {
        let (handler, credentials, sessions, recovery, codes) = handler_with_codes();
        let before = credentials.stored_hash();

        let result = handler
            .redeem(codes[0].clone(), NEW_PASSWORD.to_string(), NEW_PASSWORD.to_string())
            .await
            .unwrap();

        assert!(result.success);
        assert_ne!(credentials.stored_hash(), before, "password was not replaced");
        assert!(sessions.all_deleted(), "sessions survived a redemption");
        assert_eq!(recovery.remaining().await.unwrap(), 9);
        assert_eq!(result.recovery_codes_remaining, 9);
    }

    #[tokio::test]
    async fn given_a_code_already_used_when_redeemed_again_then_recovery_code_used() {
        let (handler, credentials, _sessions, _recovery, codes) = handler_with_codes();
        handler
            .redeem(codes[0].clone(), NEW_PASSWORD.to_string(), NEW_PASSWORD.to_string())
            .await
            .unwrap();
        let after_first = credentials.stored_hash();

        let err = handler
            .redeem(codes[0].clone(), "another good password".to_string(), "another good password".to_string())
            .await
            .unwrap_err();

        assert_eq!(rejection_code(&err), Some("recovery_code_used"));
        assert_eq!(credentials.stored_hash(), after_first, "password changed on a failed redemption");
    }

    #[tokio::test]
    async fn given_a_code_that_was_never_issued_when_redeemed_then_recovery_code_unknown() {
        let (handler, _credentials, _sessions, _recovery, _codes) = handler_with_codes();

        let err = handler
            .redeem("MNPQR-STVWX".to_string(), NEW_PASSWORD.to_string(), NEW_PASSWORD.to_string())
            .await
            .unwrap_err();

        assert_eq!(rejection_code(&err), Some("recovery_code_unknown"));
    }

    /// Decision 6: a typo in the new password must not burn a code.
    #[tokio::test]
    async fn given_a_password_below_the_policy_when_redeemed_then_no_code_is_consumed() {
        let (handler, _credentials, _sessions, recovery, codes) = handler_with_codes();

        let err = handler
            .redeem(codes[0].clone(), "short".to_string(), "short".to_string())
            .await
            .unwrap_err();

        assert!(err_is_rejection(&err));
        assert_eq!(recovery.remaining().await.unwrap(), 10, "a code was consumed by a bad password");
    }

    #[tokio::test]
    async fn given_a_confirmation_mismatch_when_redeemed_then_no_code_is_consumed() {
        let (handler, _credentials, _sessions, recovery, codes) = handler_with_codes();

        let err = handler
            .redeem(codes[0].clone(), NEW_PASSWORD.to_string(), "something else entirely".to_string())
            .await
            .unwrap_err();

        assert_eq!(rejection_code(&err), Some("password_confirmation_mismatch"));
        assert_eq!(recovery.remaining().await.unwrap(), 10);
    }

    /// A code is typed off paper; the spelling must not decide whether it works.
    #[tokio::test]
    async fn given_a_code_typed_lower_case_and_unhyphenated_when_redeemed_then_accepted() {
        let (handler, _credentials, _sessions, recovery, codes) = handler_with_codes();
        let typed = codes[0].to_lowercase().replace('-', " ");

        handler
            .redeem(typed, NEW_PASSWORD.to_string(), NEW_PASSWORD.to_string())
            .await
            .unwrap();

        assert_eq!(recovery.remaining().await.unwrap(), 9);
    }

    #[tokio::test]
    async fn given_no_account_when_redeemed_then_not_found() {
        let handler = handler_without_account();

        let err = handler
            .redeem("ABCDE-FGHJK".to_string(), NEW_PASSWORD.to_string(), NEW_PASSWORD.to_string())
            .await
            .unwrap_err();

        assert!(matches!(err, DomainError::NotFound));
    }

    #[tokio::test]
    async fn given_external_mode_when_redeemed_then_conflict() {
        let handler = handler_in_external_mode();

        let err = handler
            .redeem("ABCDE-FGHJK".to_string(), NEW_PASSWORD.to_string(), NEW_PASSWORD.to_string())
            .await
            .unwrap_err();

        assert!(matches!(err, DomainError::Conflict(_)));
    }
```

Write the `handler_with_codes`, `handler_without_account`,
`handler_in_external_mode`, `rejection_code` and `err_is_rejection` helpers to
match the fakes already in `complete_password_reset.rs`. `handler_with_codes`
returns the handler, its credential fake, its session fake, its recovery fake,
and the ten plaintext codes it seeded.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p alexandria-core --test auth recovery_redeem`
Expected: FAIL to compile — `RedeemRecoveryCodeHandler` does not exist.

- [ ] **Step 3: Implement redemption**

Create `crates/alexandria-core/src/auth/commands/redeem_recovery_code.rs`:

```rust
//! UC-43: set a new password using a recovery code (FR-AU-14 … FR-AU-16).
//!
//! The one operation that changes a password without knowing the old one, so
//! it is also the one that must not be usable by accident: it consumes the
//! code, and it logs everybody out.

use chrono::{DateTime, Utc};

use crate::auth::local::{
    LocalCredentialRepository, RecoveryCodeOutcome, RecoveryCodeRepository,
    RedeemRecoveryCodeResult, SessionRepository,
};
use crate::auth::password::{hash_password, validate_strength};
use crate::auth::recovery::hash_recovery_code;
use crate::catalog::clock::Clock;
use crate::config::AuthMode;
use crate::errors::DomainError;

/// Generic over every collaborator so the decision logic is unit-tested
/// against trait fakes and wired with the concrete ones at runtime
/// (services.rs).
pub struct RedeemRecoveryCodeHandler<CR, SR, RR, C> {
    credentials: CR,
    sessions: SR,
    recovery_codes: RR,
    clock: C,
    mode: AuthMode,
}

impl<CR, SR, RR, C> RedeemRecoveryCodeHandler<CR, SR, RR, C>
where
    CR: LocalCredentialRepository,
    SR: SessionRepository,
    RR: RecoveryCodeRepository,
    C: Clock,
{
    pub fn new(credentials: CR, sessions: SR, recovery_codes: RR, clock: C, mode: AuthMode) -> Self {
        Self {
            credentials,
            sessions,
            recovery_codes,
            clock,
            mode,
        }
    }

    /// Replace the password using one recovery code.
    ///
    /// The order of the checks is the point. The password is validated before
    /// the code table is touched, so a typo in the new password leaves every
    /// code intact and the owner can try the same one again (FR-AU-16). Only
    /// once the password is known-good is a code spent.
    pub async fn redeem(
        &self,
        code: String,
        new_password: String,
        password_confirmation: String,
    ) -> Result<RedeemRecoveryCodeResult, DomainError> {
        // AF-01: the active auth mode must be local login (FR-AU-03).
        if self.mode != AuthMode::Local {
            return Err(DomainError::conflict(
                "local login is not the active auth mode",
            ));
        }

        // AF-02: there must be an account to recover.
        let credential = self.credentials.get().await?.ok_or(DomainError::NotFound)?;

        // AF-03/AF-04: the new password must satisfy the policy and match its
        // confirmation. Both are pure checks over the input — no read, no
        // write — so reaching them costs nothing and failing them costs no
        // code.
        validate_strength(&new_password, &credential.email)?;
        if new_password != password_confirmation {
            return Err(DomainError::rejected(
                "password_confirmation_mismatch",
                "password confirmation does not match the password",
            ));
        }

        // AF-05/AF-06: spend the code. The two rejections are deliberately
        // distinguishable — someone working down a printed list needs to know
        // whether they mistyped or already spent it (FR-AU-15).
        let now: DateTime<Utc> = self.clock.now();
        match self
            .recovery_codes
            .consume(&hash_recovery_code(&code), now)
            .await?
        {
            RecoveryCodeOutcome::Consumed => {}
            RecoveryCodeOutcome::AlreadyUsed => {
                return Err(DomainError::rejected(
                    "recovery_code_used",
                    "that recovery code has already been used",
                ))
            }
            RecoveryCodeOutcome::Unknown => {
                return Err(DomainError::rejected(
                    "recovery_code_unknown",
                    "that recovery code is not one of this account's",
                ))
            }
        }

        let password_hash = hash_password(&new_password)?;
        self.credentials
            .upsert(&credential.email, &password_hash, now)
            .await?;

        // FR-AU-14: a redemption is what an owner does when they may have lost
        // control of the account. Leaving somebody else's session open would
        // defeat the whole point.
        self.sessions.delete_all().await?;

        Ok(RedeemRecoveryCodeResult {
            success: true,
            email: credential.email,
            recovery_codes_remaining: self.recovery_codes.remaining().await?,
        })
    }
}
```

Add to `crates/alexandria-core/src/auth/local.rs`:

```rust
/// The outcome of redeeming a recovery code (UC-43 / FR-AU-14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RedeemRecoveryCodeResult {
    pub success: bool,
    pub email: String,
    /// What is left after this redemption. Zero means the next forgotten
    /// password is unrecoverable, so a client should prompt to regenerate.
    pub recovery_codes_remaining: u32,
}

/// The outcome of regenerating the set (UC-44 / FR-AU-17).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegenerateRecoveryCodesResult {
    /// The new codes in plaintext, returned exactly once.
    pub recovery_codes: Vec<String>,
}
```

- [ ] **Step 4: Run the redemption tests to verify they pass**

Run: `cargo test -p alexandria-core --test auth recovery_redeem`
Expected: PASS, 8 tests.

- [ ] **Step 5: Write the failing tests for regeneration**

Create `crates/alexandria-core/tests/auth/recovery_regenerate.rs` and declare
it in `tests/auth.rs`. Model the helpers on `tests/auth/set_credentials.rs`,
the closest authenticated-command analogue.

```rust
use alexandria_core::auth::commands::regenerate_recovery_codes::RegenerateRecoveryCodesHandler;
use alexandria_core::auth::local::{RecoveryCodeOutcome, RecoveryCodeRepository};
use alexandria_core::auth::recovery::hash_recovery_code;
use alexandria_core::errors::DomainError;
use chrono::Utc;

    #[tokio::test]
    async fn given_an_authenticated_owner_when_regenerated_then_ten_new_codes() {
        let (handler, recovery, old_codes) = handler_with_codes();

        let result = handler.regenerate("session").await.unwrap();

        assert_eq!(result.recovery_codes.len(), 10);
        assert_eq!(recovery.remaining().await.unwrap(), 10);
        for old in &old_codes {
            assert!(!result.recovery_codes.contains(old));
        }
    }

    /// FR-AU-17: an unused code from the old set must stop working, or the
    /// owner cannot tell which of their written codes are live.
    #[tokio::test]
    async fn given_unused_old_codes_when_regenerated_then_they_no_longer_work() {
        let (handler, recovery, old_codes) = handler_with_codes();

        handler.regenerate("session").await.unwrap();

        assert_eq!(
            recovery
                .consume(&hash_recovery_code(&old_codes[7]), Utc::now())
                .await
                .unwrap(),
            RecoveryCodeOutcome::Unknown
        );
    }

    #[tokio::test]
    async fn given_an_unauthenticated_caller_when_regenerating_then_unauthorized() {
        let handler = handler_rejecting_auth();

        let err = handler.regenerate("nonsense").await.unwrap_err();

        assert!(matches!(err, DomainError::Unauthorized));
    }

    #[tokio::test]
    async fn given_external_mode_when_regenerating_then_conflict() {
        let handler = handler_in_external_mode();

        let err = handler.regenerate("session").await.unwrap_err();

        assert!(matches!(err, DomainError::Conflict(_)));
    }
```

- [ ] **Step 6: Run them to verify they fail**

Run: `cargo test -p alexandria-core --test auth recovery_regenerate`
Expected: FAIL to compile — `RegenerateRecoveryCodesHandler` does not exist.

- [ ] **Step 7: Implement regeneration**

Create `crates/alexandria-core/src/auth/commands/regenerate_recovery_codes.rs`:

```rust
//! UC-44: replace the owner's recovery codes with a fresh set (FR-AU-17).
//!
//! Without this, recovery is finite — ten redemptions and the account is
//! unrecoverable again, which is the state recovery codes exist to escape.
//! It is also the only answer to a printed list going missing.

use crate::auth::local::{
    LocalCredentialRepository, RecoveryCodeRepository, RegenerateRecoveryCodesResult,
};
use crate::auth::recovery::{generate_recovery_codes, hash_recovery_code};
use crate::auth::AuthService;
use crate::catalog::clock::Clock;
use crate::config::AuthMode;
use crate::errors::DomainError;

pub struct RegenerateRecoveryCodesHandler<A, CR, RR, C> {
    auth: A,
    credentials: CR,
    recovery_codes: RR,
    clock: C,
    mode: AuthMode,
}

impl<A, CR, RR, C> RegenerateRecoveryCodesHandler<A, CR, RR, C>
where
    A: AuthService,
    CR: LocalCredentialRepository,
    RR: RecoveryCodeRepository,
    C: Clock,
{
    pub fn new(auth: A, credentials: CR, recovery_codes: RR, clock: C, mode: AuthMode) -> Self {
        Self {
            auth,
            credentials,
            recovery_codes,
            clock,
            mode,
        }
    }

    /// Issue a fresh set, invalidating every existing code.
    ///
    /// Authenticated, unlike redemption: this is the owner who still has
    /// access, topping up before they need it. Every old code dies, used or
    /// not — a partial refill would leave them unsure which of their written
    /// codes still work.
    pub async fn regenerate(
        &self,
        token: &str,
    ) -> Result<RegenerateRecoveryCodesResult, DomainError> {
        // AF-02: the caller must be the authenticated owner.
        self.auth.authenticate(token).await?;

        // AF-01: the active auth mode must be local login (FR-AU-03).
        if self.mode != AuthMode::Local {
            return Err(DomainError::conflict(
                "local login is not the active auth mode",
            ));
        }

        // AF-03: there must be an account to hold the codes.
        if self.credentials.get().await?.is_none() {
            return Err(DomainError::NotFound);
        }

        let recovery_codes = generate_recovery_codes();
        let hashes: Vec<String> = recovery_codes.iter().map(|c| hash_recovery_code(c)).collect();
        self.recovery_codes
            .replace_all(&hashes, self.clock.now())
            .await?;

        Ok(RegenerateRecoveryCodesResult { recovery_codes })
    }
}
```

Add both modules to `crates/alexandria-core/src/auth/commands/mod.rs`,
alphabetically.

- [ ] **Step 8: Run both suites to verify they pass**

Run: `cargo test -p alexandria-core recovery`
Expected: PASS — 12 new handler tests plus Task 1's and Task 2's.

- [ ] **Step 9: Add the HTTP routes**

In `crates/alexandria-http/src/routes/auth.rs`, add two handlers in the style
of the existing ones — request structs deriving `Deserialize` with
`#[serde(rename_all = "camelCase")]`, and doc comments naming the use case:

- `POST /v1/auth/local/recovery/redeem` — body `{ code, newPassword, passwordConfirmation }`, unauthenticated, answering `200` with `RedeemRecoveryCodeResult`.
- `POST /v1/auth/local/recovery/regenerate` — no body, authenticated by the same bearer/session extraction the other authenticated routes use, answering `200` with `RegenerateRecoveryCodesResult`.

Register both in the same router builder as the existing `/v1/auth/local/*`
routes. Note the redeem route must sit **outside** the authenticated
`route_layer`, alongside `login` and `register` — its whole purpose is to serve
a caller who cannot authenticate.

- [ ] **Step 10: Add the FFI exports**

In `crates/alexandria-ffi/src/lib.rs`, add
`alexandria_auth_local_redeem_recovery_code(json_body: *const c_char) -> AuthJsonResult`
and
`alexandria_auth_local_regenerate_recovery_codes(token: *const c_char) -> AuthJsonResult`,
copying the shape of `alexandria_auth_local_complete_password_reset` and
`alexandria_auth_local_account` respectively.

- [ ] **Step 11: Add parity tests**

In `crates/alexandria-ffi/tests/parity.rs`, add a parity assertion per new
operation, following the file's existing pattern: drive the HTTP route and the
FFI export against equivalent state and assert the JSON bodies match. Cover
both a success and one rejection (`recovery_code_unknown`), since FR-AU-12
requires the reason code to be identical on both surfaces.

- [ ] **Step 12: Run the whole workspace**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 13: Check lints and formatting**

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check`
Expected: clean.

- [ ] **Step 14: Commit**

```bash
git add -A
git commit -m "feat: redeem and regenerate recovery codes"
```

---

### Task 5: Remove the e-mail machinery

Everything the previous tasks made redundant, plus the migration that drops its
tables. Nothing here is additive — the whole task is deletion, so a reviewer can
judge it on whether anything of value went with it.

**Files:**
- Delete: `crates/alexandria-core/src/auth/mail.rs`, `crates/alexandria-core/src/auth/tokens.rs`, `crates/alexandria-core/src/auth/commands/{confirm_email,resend_confirmation,request_password_reset,complete_password_reset}.rs`, and their test files under `crates/alexandria-core/tests/auth/`
- Create: `crates/alexandria-core/migrations/00000000000014_drop_email_recovery.sql`
- Modify: `crates/alexandria-core/src/auth/{mod.rs,local.rs}`, `commands/mod.rs`, `commands/register.rs`, `commands/account_status.rs`, `config.rs`, `services.rs`, `crates/alexandria-http/src/routes/auth.rs`, `crates/alexandria-ffi/src/lib.rs`, `config.toml.example`

**Interfaces:**
- Consumes: everything from Tasks 1–4.
- Produces: `LocalRegisterResult` loses `email_confirmed`, `confirmation_sent`, `confirmation_error`; `LocalAccountResult` loses `email_confirmed`; `LocalCredential` loses `email_confirmed_at` and its `email_confirmed()` method; `AuthSettings` loses three keys; `MailProvider`, `MailSettings` and the `[mail]` section are gone.

- [ ] **Step 1: Write the migration**

Create `crates/alexandria-core/migrations/00000000000014_drop_email_recovery.sql`:

```sql
-- Recovery codes replaced e-mail confirmation and password reset, so the
-- state they needed goes with them.
--
-- Nothing of value is dropped. `MailProvider` has only ever had one variant,
-- `None`, so every send was refused: no row in `auth_tokens` was ever
-- delivered to anyone, and `email_confirmed_at` is NULL on every install in
-- existence because nothing could ever have confirmed an address.
DROP TABLE IF EXISTS auth_tokens;

ALTER TABLE local_login_credentials DROP COLUMN email_confirmed_at;
```

If the bundled SQLite rejects `DROP COLUMN`, fall back to the
create-copy-drop-rename dance in one transaction and say so in your report —
do not leave the column in place.

- [ ] **Step 2: Delete the commands and their tests**

```bash
git rm crates/alexandria-core/src/auth/mail.rs \
       crates/alexandria-core/src/auth/tokens.rs \
       crates/alexandria-core/src/auth/commands/confirm_email.rs \
       crates/alexandria-core/src/auth/commands/resend_confirmation.rs \
       crates/alexandria-core/src/auth/commands/request_password_reset.rs \
       crates/alexandria-core/src/auth/commands/complete_password_reset.rs
```

Then remove their `pub mod` lines from `auth/mod.rs` and
`auth/commands/mod.rs`, and delete any file under
`crates/alexandria-core/tests/auth/` that tests only the deleted commands,
removing its `mod` declaration from `tests/auth.rs`.

- [ ] **Step 3: Strip the confirmation state**

In `crates/alexandria-core/src/auth/local.rs`: delete `email_confirmed_at` from
`LocalCredential` and its `email_confirmed()` method, delete
`ConfirmEmailResult` and `ResendConfirmationResult` and any other result type
belonging only to the deleted commands, and remove `email_confirmed` from
`LocalAccountResult` and `email_confirmed` / `confirmation_sent` /
`confirmation_error` from `LocalRegisterResult`. Update the SQL in
`SqliteLocalCredentialRepository` so it no longer selects or writes the dropped
column.

In `register.rs`, delete the `tokens`, `mail` and `confirmation_ttl_hours`
fields, the `send_confirmation` method, and the three fields from the returned
result. In `account_status.rs`, delete the `email_confirmed` field from the
result it builds.

- [ ] **Step 4: Strip the configuration**

In `crates/alexandria-core/src/config.rs`, delete `confirmation_ttl_hours`,
`password_reset_ttl_minutes` and `resend_interval_seconds` (fields, defaults,
`Default` initializers, and their `ALEXANDRIA_AUTH_*` overrides), and delete
`MailProvider`, `MailSettings`, the `mail` field on `Settings` and its
`ALEXANDRIA_MAIL_FROM_ADDRESS` override.

In `config.toml.example`, delete those three keys with their comment blocks and
the whole `[mail]` section.

- [ ] **Step 5: Strip the transports**

In `crates/alexandria-http/src/routes/auth.rs`, delete the four route handlers
and their request structs, and their registrations in the router.

In `crates/alexandria-ffi/src/lib.rs`, delete
`alexandria_auth_local_confirm_email`,
`alexandria_auth_local_resend_confirmation`,
`alexandria_auth_local_request_password_reset` and
`alexandria_auth_local_complete_password_reset`.

In `services.rs`, delete the token repository and mail sender construction and
every handler alias for a deleted command.

- [ ] **Step 6: Fix the fallout and run everything**

Run: `cargo test --workspace`

Existing HTTP, FFI and parity tests assert on the removed fields and call the
removed operations — delete those assertions and tests. A test that exists only
to exercise a deleted operation goes with it; a test that merely mentions a
removed field loses the field. Repeat until green.

Expected: PASS.

- [ ] **Step 7: Confirm nothing references the removed machinery**

Run: `grep -rn "confirmation\|password_reset\|email_confirmed\|MailProvider\|auth_tokens\|TokenPurpose" --include=*.rs --include=*.toml crates config.toml.example`
Expected: no matches, except `password_confirmation` / `passwordConfirmation`,
which is the register and redeem field name and stays.

- [ ] **Step 8: Check lints and formatting**

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check`
Expected: clean.

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "feat: drop e-mail confirmation and reset"
```

---

### Task 6: Documentation

**Files:**
- Modify: `docs/requirements/System Requirements Document.md`, `docs/requirements/Use Case Specification Document.md`, `docs/requirements/Operations & Infrastructure Document.md`, `docs/requirements/Testing Specification Document.md`, `README.md`

- [ ] **Step 1: Replace FR-AU-13 … FR-AU-19**

In `docs/requirements/System Requirements Document.md`, replace the seven rows
with these, keeping the table's format:

| ID | Requirement |
| --- | --- |
| FR-AU-13 | On registration the system shall generate ten single-use recovery codes, return them to the caller exactly once, and store only their hashes. |
| FR-AU-14 | The system shall replace the local password on presentation of an unconsumed recovery code together with a new password satisfying FR-AU-11, shall consume that code, and shall invalidate every existing session. |
| FR-AU-15 | The system shall reject a presented recovery code with a reason that distinguishes an unrecognised code from one already consumed. |
| FR-AU-16 | The system shall not consume a recovery code when the redemption fails for any other reason. |
| FR-AU-17 | The system shall, for an authenticated owner, replace every recovery code with ten new ones and return them exactly once. |
| FR-AU-18 | The system shall report to an authenticated owner how many recovery codes remain unconsumed. |
| FR-AU-19 | The system shall store only a hash of every recovery code; the plaintext shall exist only in the response that issues it. |

Then update §4's data model: remove the `emailConfirmedAt` row and the
`auth_tokens` table description, and add `recovery_codes` with the four columns
from Task 2's migration. Update the §7 endpoint table: remove the four e-mail
rows, add `POST /v1/auth/local/recovery/redeem` and
`POST /v1/auth/local/recovery/regenerate`, and correct the
`/v1/auth/local/account` row's requirement reference to FR-AU-18.

- [ ] **Step 2: Write UC-43 and UC-44**

In `docs/requirements/Use Case Specification Document.md`, delete any use case
covering e-mail confirmation or password reset, and add these after UC-42,
following the document's exact field and table format:

**UC-43: Redeem a recovery code** — Actors: Owner. Preconditions: the active
auth mode is local login; an account exists; the caller holds one of its
recovery codes. Postconditions: the password is replaced, that code is
consumed, and every session is invalidated. Requirements: FR-AU-11, FR-AU-14,
FR-AU-15, FR-AU-16.

Main flow: (1) the caller submits a recovery code, a new password, and a
confirmation; (2) the system confirms the active mode is local login; (3) the
system confirms an account exists; (4) the system validates the new password
against the strength policy and its confirmation; (5) the system consumes the
code; (6) the system replaces the stored password hash and deletes every
session; (7) the system reports how many codes remain.

| ID | Condition | Outcome |
| --- | --- | --- |
| AF-01 | The active auth mode is external JWT | The system rejects with an invalid-operation error. |
| AF-02 | No local account exists | The system responds with a not-found error. |
| AF-03 | The new password fails the strength policy | The system rejects naming the unmet rule; no code is consumed. |
| AF-04 | The confirmation does not match the new password | The system rejects; no code is consumed. |
| AF-05 | The code was already used | The system rejects with `recovery_code_used`; the password is unchanged. |
| AF-06 | The code was never issued, or belongs to a regenerated-away set | The system rejects with `recovery_code_unknown`. |

Add below the table: the checks run in the order listed, so a rejected password
never reaches the code table — a typo in the new password must not spend a code
the owner may have only one of.

**UC-44: Regenerate recovery codes** — Actors: Owner. Preconditions: the active
auth mode is local login; the caller is authenticated; an account exists.
Postconditions: every previous code is invalid and ten new ones exist, returned
once. Requirements: FR-AU-17, FR-AU-19.

Main flow: (1) the authenticated owner requests a new set; (2) the system
confirms the caller is authenticated; (3) the system confirms the active mode
is local login; (4) the system confirms an account exists; (5) the system
replaces every code with ten new ones and returns them.

| ID | Condition | Outcome |
| --- | --- | --- |
| AF-01 | The caller is not authenticated | The system denies with an unauthorized error. |
| AF-02 | The active auth mode is external JWT | The system rejects with an invalid-operation error. |
| AF-03 | No local account exists | The system responds with a not-found error. |

Add both to the §3 traceability table, and update UC-41's row and postconditions
to mention the recovery codes it now returns.

- [ ] **Step 3: Update the operations document**

In `docs/requirements/Operations & Infrastructure Document.md`, remove
`auth.confirmation_ttl_hours`, `auth.password_reset_ttl_minutes`,
`auth.resend_interval_seconds`, `mail.provider` and `mail.from_address` from
the §4 configuration table, and delete any runbook passage about mail delivery.

Add a short passage under the authentication section:

```markdown
#### Recovery codes (local mode)

Registration returns ten single-use recovery codes. They are shown once and
stored only as hashes, so there is no way to recover them afterwards — an
owner who loses both their password and their codes has no route back into
the catalog short of editing the database by hand.

Redeeming a code sets a new password and signs every session out. An owner who
is running low regenerates from `POST /v1/auth/local/recovery/regenerate`,
which invalidates the whole previous set, used codes and unused alike.

An account created before this feature holds no codes:
`GET /v1/auth/local/account` reports `recoveryCodesRemaining: 0`. Its owner
should log in and regenerate while they still know their password.
```

- [ ] **Step 4: Update the testing document**

In `docs/requirements/Testing Specification Document.md` §6.2, replace any test
double row naming a mail sender or token repository with a recovery code
repository row: hand-written in-memory fake implementing the trait.

- [ ] **Step 5: Update the README**

In `README.md`'s F-09 table, remove rows for the deleted e-mail operations and
add UC-43 and UC-44 with their requirement references, following the table's
existing format and checkbox convention.

- [ ] **Step 6: Verify no stale references survive**

Run: `grep -rni "confirmation code\|password reset\|resend\|mail provider\|jwks" README.md docs/requirements config.toml.example`
Expected: no matches.

- [ ] **Step 7: Commit**

```bash
git add README.md docs/requirements config.toml.example
git commit -m "docs: describe local recovery codes"
```

---

## Self-review notes

Checked against the spec:

- Decision 1 (confirmation removed with reset) — Task 5, including the migration and the config.
- Decision 2 (code + password in one call, sessions cleared) — Task 4, with tests for the replacement, the session wipe, and the remaining count.
- Decision 3 (ten codes, once) — Tasks 1 and 3; "no stored value is a code" is Task 3's second test.
- Decision 4 (SHA-256, not Argon2) — Task 1, with the hash-shape test.
- Decision 5 (regeneration invalidates unused codes) — Task 4, plus Task 2's repository-level test.
- Decision 6 (password validated before the code table) — Task 4's implementation ordering, pinned by two tests that assert `remaining() == 10` after a failure.
- Decision 7 (distinct reason codes) — `RecoveryCodeOutcome`'s three states in Task 2, mapped in Task 4, asserted in both.
- The migration and its upgrade consequence — Tasks 2 and 5; the zero-count case is tested in Task 3 and documented in Task 6.
- FR-AU-08 (both surfaces) — Task 4 steps 9–11, including parity for a rejection.

Deliberately absent, per the spec's out-of-scope section: Windows credential
login, any outbound mail, and rate-limiting redemption.
