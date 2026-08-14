//! Single-use, expiring tokens for e-mail confirmation and password reset
//! (issue #102, FR-AU-17, FR-AU-18).
//!
//! One table serves both purposes: they differ only in what is generated, how
//! long it lives, and what consuming it does. The parts that must not drift —
//! expiry, single use, and the three outcomes a presented value can have — are
//! written once, here.

use argon2::password_hash::rand_core::{OsRng, RngCore};
use chrono::{DateTime, Utc};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use crate::catalog::fs::sha256_hex;
use crate::errors::DomainError;

/// What a token authorizes. Stored as a string so a row is readable, and
/// matched on read — a confirmation code presented to the reset endpoint is
/// not a valid reset token, however genuine it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenPurpose {
    EmailConfirmation,
    PasswordReset,
}

impl TokenPurpose {
    pub fn as_str(self) -> &'static str {
        match self {
            TokenPurpose::EmailConfirmation => "email_confirmation",
            TokenPurpose::PasswordReset => "password_reset",
        }
    }
}

/// A stored token row. Never carries the plaintext — only the hash of it was
/// ever written (FR-AU-19).
#[derive(Debug, Clone)]
pub struct AuthToken {
    pub id: i64,
    pub purpose: String,
    pub email: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub consumed_at: Option<DateTime<Utc>>,
}

/// What a presented code or token turned out to be.
///
/// The three failures are distinct on purpose (FR-AU-18). Telling them apart
/// leaks nothing to someone who already holds the value, and "that one has
/// expired, ask for another" is a materially different instruction to an owner
/// than "that code is wrong".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenOutcome {
    Valid {
        id: i64,
        email: String,
    },
    /// No such token for this purpose.
    Invalid,
    /// A real token that has already been used.
    AlreadyUsed,
    /// A real token whose lifetime has elapsed.
    Expired,
}

/// The alphabet confirmation codes are drawn from: Crockford base32 without
/// `I`, `L`, `O`, and `U` — the characters a person retyping a code from a
/// message confuses with `1`, `0`, or mistypes into a word.
const CODE_ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Characters in a confirmation code. Eight of a 32-character alphabet is 40
/// bits — short enough to retype, and with single use plus the resend interval
/// (FR-AU-15) far past the point where guessing is a strategy.
const CODE_LENGTH: usize = 8;

/// Bytes of randomness in a password-reset token. Nothing retypes it — it
/// travels in a link — so it is sized for the threat rather than for a person.
const RESET_TOKEN_BYTES: usize = 32;

/// Generate a confirmation code: `CODE_LENGTH` characters of `CODE_ALPHABET`,
/// drawn from the OS entropy source.
///
/// The alphabet's length divides 256 evenly, so masking a random byte down to
/// 5 bits is uniform — no modulo bias, and no rejection loop to get it right.
pub fn generate_confirmation_code() -> String {
    let mut bytes = [0u8; CODE_LENGTH];
    OsRng.fill_bytes(&mut bytes);
    bytes
        .iter()
        .map(|byte| CODE_ALPHABET[usize::from(byte & 0x1f)] as char)
        .collect()
}

/// Generate a password-reset token: `RESET_TOKEN_BYTES` of OS entropy, hex
/// encoded. Hex rather than base64 keeps it URL-safe with no encoding
/// dependency, at the cost of length nobody reads.
pub fn generate_reset_token() -> String {
    use std::fmt::Write as _;

    let mut bytes = [0u8; RESET_TOKEN_BYTES];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// Hash a presented code or token the way it was stored.
///
/// SHA-256, not Argon2. The stored password hash defends a low-entropy secret
/// a person chose, so it must be slow; these values are 40 and 256 bits of OS
/// entropy, where a brute-force is already out of reach and the cost of a slow
/// hash would only be paid on every lookup.
pub fn hash_token(value: &str) -> String {
    sha256_hex(value.trim().as_bytes())
}

/// Token storage port (issue #102). Unit-testable against an in-memory fake
/// with no database (Testing Specification §6.2).
#[allow(async_fn_in_trait)]
pub trait AuthTokenRepository: Send + Sync {
    /// Store a freshly minted token's hash, returning its row id.
    async fn insert(
        &self,
        purpose: TokenPurpose,
        token_hash: &str,
        email: &str,
        created_at: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Result<i64, DomainError>;

    /// Delete a token outright.
    ///
    /// Not the same as consuming it. This is for a token that was recorded and
    /// then never left the building — the send meant to carry it failed — so
    /// it must leave no trace at all: not a usable code, and not a row that
    /// makes the next resend look too soon (FR-AU-15).
    async fn delete(&self, id: i64) -> Result<(), DomainError>;

    /// The row whose stored hash equals `token_hash`, if any.
    async fn find_by_hash(&self, token_hash: &str) -> Result<Option<AuthToken>, DomainError>;

    /// Mark `id` used, returning `false` if it already was.
    ///
    /// Atomic by construction: reading `consumed_at` and then writing it would
    /// be check-then-act, and two concurrent presentations of the same token
    /// could both pass the check — exactly the double-use single-use exists to
    /// prevent.
    async fn consume(&self, id: i64, consumed_at: DateTime<Utc>) -> Result<bool, DomainError>;

    /// When the most recent token of `purpose` was created, for the resend
    /// interval (FR-AU-15).
    async fn last_created_at(
        &self,
        purpose: TokenPurpose,
    ) -> Result<Option<DateTime<Utc>>, DomainError>;

    /// Mark every outstanding token of `purpose` used.
    ///
    /// Called after a successful confirm or reset: once one code has done its
    /// job, every earlier one still in an inbox must stop working.
    async fn invalidate_outstanding(
        &self,
        purpose: TokenPurpose,
        consumed_at: DateTime<Utc>,
    ) -> Result<(), DomainError>;
}

/// Resolve a presented value against storage into one of the four outcomes.
///
/// Shared by confirm and reset so the ordering — unknown, then used, then
/// expired — is decided once. Ordering matters: a token that is both used and
/// expired reports `AlreadyUsed`, which is the more useful thing to tell an
/// owner ("that already worked") than "it aged out".
pub async fn resolve_token<R>(
    tokens: &R,
    purpose: TokenPurpose,
    presented: &str,
    now: DateTime<Utc>,
) -> Result<TokenOutcome, DomainError>
where
    R: AuthTokenRepository,
{
    let Some(token) = tokens.find_by_hash(&hash_token(presented)).await? else {
        return Ok(TokenOutcome::Invalid);
    };
    if token.purpose != purpose.as_str() {
        return Ok(TokenOutcome::Invalid);
    }
    if token.consumed_at.is_some() {
        return Ok(TokenOutcome::AlreadyUsed);
    }
    if now >= token.expires_at {
        return Ok(TokenOutcome::Expired);
    }
    Ok(TokenOutcome::Valid {
        id: token.id,
        email: token.email,
    })
}

#[derive(Clone)]
pub struct SqliteAuthTokenRepository {
    pool: SqlitePool,
}

impl SqliteAuthTokenRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

/// Parse a stored RFC 3339 timestamp, naming the column when it is corrupt.
fn parse_timestamp(value: &str, column: &str) -> Result<DateTime<Utc>, DomainError> {
    DateTime::parse_from_rfc3339(value)
        .map(|parsed| parsed.with_timezone(&Utc))
        .map_err(|err| DomainError::internal(format!("corrupt auth_tokens.{column}: {err}")))
}

impl AuthTokenRepository for SqliteAuthTokenRepository {
    async fn insert(
        &self,
        purpose: TokenPurpose,
        token_hash: &str,
        email: &str,
        created_at: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Result<i64, DomainError> {
        let result = sqlx::query(
            "INSERT INTO auth_tokens (purpose, token_hash, email, created_at, expires_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(purpose.as_str())
        .bind(token_hash)
        .bind(email)
        .bind(created_at.to_rfc3339())
        .bind(expires_at.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(result.last_insert_rowid())
    }

    async fn delete(&self, id: i64) -> Result<(), DomainError> {
        sqlx::query("DELETE FROM auth_tokens WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn find_by_hash(&self, token_hash: &str) -> Result<Option<AuthToken>, DomainError> {
        let row = sqlx::query(
            "SELECT id, purpose, email, created_at, expires_at, consumed_at \
             FROM auth_tokens WHERE token_hash = ?",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };
        let created_at: String = row.try_get("created_at")?;
        let expires_at: String = row.try_get("expires_at")?;
        let consumed_at: Option<String> = row.try_get("consumed_at")?;
        Ok(Some(AuthToken {
            id: row.try_get("id")?,
            purpose: row.try_get("purpose")?,
            email: row.try_get("email")?,
            created_at: parse_timestamp(&created_at, "created_at")?,
            expires_at: parse_timestamp(&expires_at, "expires_at")?,
            consumed_at: consumed_at
                .map(|value| parse_timestamp(&value, "consumed_at"))
                .transpose()?,
        }))
    }

    async fn consume(&self, id: i64, consumed_at: DateTime<Utc>) -> Result<bool, DomainError> {
        let result = sqlx::query(
            "UPDATE auth_tokens SET consumed_at = ? WHERE id = ? AND consumed_at IS NULL",
        )
        .bind(consumed_at.to_rfc3339())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn last_created_at(
        &self,
        purpose: TokenPurpose,
    ) -> Result<Option<DateTime<Utc>>, DomainError> {
        let row = sqlx::query(
            "SELECT created_at FROM auth_tokens WHERE purpose = ? \
             ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .bind(purpose.as_str())
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };
        let created_at: String = row.try_get("created_at")?;
        Ok(Some(parse_timestamp(&created_at, "created_at")?))
    }

    async fn invalidate_outstanding(
        &self,
        purpose: TokenPurpose,
        consumed_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        sqlx::query(
            "UPDATE auth_tokens SET consumed_at = ? \
             WHERE purpose = ? AND consumed_at IS NULL",
        )
        .bind(consumed_at.to_rfc3339())
        .bind(purpose.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
