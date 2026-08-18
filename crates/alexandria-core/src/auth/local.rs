use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use uuid::Uuid;

use crate::auth::{AuthService, Principal};
use crate::catalog::clock::Clock;
use crate::config::AuthMode;
use crate::errors::DomainError;

/// Confirmation that local-login credentials were set (UC-35 / FR-AU-05).
/// Never carries the password or its hash (FR-AU-06).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalCredentialsResult {
    pub success: bool,
    pub email: String,
}

/// Confirmation that a local login succeeded (UC-34), carrying the session
/// id the caller presents on subsequent requests instead of a bearer token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalLoginResult {
    pub success: bool,
    pub session_id: Uuid,
}

/// The authenticated owner's account state (FR-AU-18). What the front-end's
/// catalog lock reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalAccountResult {
    pub email: String,
    /// How many recovery codes are still unspent (FR-AU-18).
    ///
    /// Zero means the account cannot currently be recovered — either every
    /// code has been used, or the account predates them — and the owner
    /// should regenerate while they still know their password.
    pub recovery_codes_remaining: u32,
}

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
    /// The owner's recovery codes, in plaintext, returned exactly once
    /// (FR-AU-13). They are not retrievable afterwards: only their hashes are
    /// stored, so this response is the only chance to record them.
    pub recovery_codes: Vec<String>,
}

/// The single owner's local-login credential row (SRD §4.9). Singleton —
/// there is exactly one row, `id = 1`.
#[derive(Debug, Clone)]
pub struct LocalCredential {
    pub email: String,
    pub password_hash: String,
}

/// Local-login credentials repository port (UC-34/UC-35). Unit-testable
/// against an in-memory fake with no database (Testing Specification §6.2).
#[allow(async_fn_in_trait)]
pub trait LocalCredentialRepository: Send + Sync {
    /// The current credential row, if local login has been set up (UC-34
    /// AF-03: `None` means "run UC-41 first").
    async fn get(&self) -> Result<Option<LocalCredential>, DomainError>;
    /// Create or overwrite the singleton credential row (UC-35 / FR-AU-05).
    async fn upsert(
        &self,
        email: &str,
        password_hash: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<(), DomainError>;
    /// Create the singleton credential row only if it does not already
    /// exist, returning `true` when this call created it and `false` when a
    /// row was already there (UC-41 / FR-AU-10). `get()` followed by
    /// `upsert` is check-then-act: two concurrent first-time registrations
    /// can both pass the `get()` check before either writes, and the second
    /// `upsert` would silently overwrite the first — exactly the
    /// silent-overwrite failure UC-41 exists to eliminate. This method
    /// closes that window by making the existence check and the write a
    /// single atomic operation at the storage layer.
    async fn insert_if_absent(
        &self,
        email: &str,
        password_hash: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<bool, DomainError>;
    /// Replace the stored password after a completed recovery-code
    /// redemption (FR-AU-14). Takes no address: this changes the
    /// credential, never who the account belongs to.
    async fn set_password_hash(
        &self,
        password_hash: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<(), DomainError>;
}

/// Sessions repository port (UC-34 postcondition: "a session must be
/// created to keep track of the login"). A local login creates a session;
/// every subsequent request in local mode is authenticated by presenting
/// that session's id instead of a bearer token.
#[allow(async_fn_in_trait)]
pub trait SessionRepository: Send + Sync {
    /// Persist a new session, valid until `expires_at`.
    async fn create_session(
        &self,
        id: Uuid,
        created_at: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Result<(), DomainError>;
    /// Whether `id` names a session that has not yet expired as of `now`.
    async fn is_valid(&self, id: Uuid, now: DateTime<Utc>) -> Result<bool, DomainError>;
    /// Delete every session (FR-AU-14).
    ///
    /// Called after a recovery-code redemption. Redeeming a code is what an
    /// owner does when they believe someone else may hold their credentials;
    /// leaving that someone's session open would defeat the whole point.
    async fn delete_all(&self) -> Result<(), DomainError>;
}

#[derive(Clone)]
pub struct SqliteLocalCredentialRepository {
    pool: SqlitePool,
}

impl SqliteLocalCredentialRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

impl LocalCredentialRepository for SqliteLocalCredentialRepository {
    async fn get(&self) -> Result<Option<LocalCredential>, DomainError> {
        let row =
            sqlx::query("SELECT email, password_hash FROM local_login_credentials WHERE id = 1")
                .fetch_optional(&self.pool)
                .await?;

        let Some(row) = row else {
            return Ok(None);
        };
        Ok(Some(LocalCredential {
            email: row.try_get("email")?,
            password_hash: row.try_get("password_hash")?,
        }))
    }

    async fn upsert(
        &self,
        email: &str,
        password_hash: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        sqlx::query(
            "INSERT INTO local_login_credentials (id, email, password_hash, updated_at) \
             VALUES (1, ?, ?, ?) \
             ON CONFLICT (id) DO UPDATE SET \
                email = excluded.email, \
                password_hash = excluded.password_hash, \
                updated_at = excluded.updated_at",
        )
        .bind(email)
        .bind(password_hash)
        .bind(updated_at.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn insert_if_absent(
        &self,
        email: &str,
        password_hash: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<bool, DomainError> {
        let result = sqlx::query(
            "INSERT INTO local_login_credentials (id, email, password_hash, updated_at) \
             VALUES (1, ?, ?, ?) \
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(email)
        .bind(password_hash)
        .bind(updated_at.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn set_password_hash(
        &self,
        password_hash: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        sqlx::query(
            "UPDATE local_login_credentials SET password_hash = ?, updated_at = ? WHERE id = 1",
        )
        .bind(password_hash)
        .bind(updated_at.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct SqliteSessionRepository {
    pool: SqlitePool,
}

impl SqliteSessionRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

impl SessionRepository for SqliteSessionRepository {
    async fn create_session(
        &self,
        id: Uuid,
        created_at: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        sqlx::query("INSERT INTO sessions (id, created_at, expires_at) VALUES (?, ?, ?)")
            .bind(id.to_string())
            .bind(created_at.to_rfc3339())
            .bind(expires_at.to_rfc3339())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn is_valid(&self, id: Uuid, now: DateTime<Utc>) -> Result<bool, DomainError> {
        let row = sqlx::query("SELECT expires_at FROM sessions WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;

        let Some(row) = row else {
            return Ok(false);
        };
        let expires_at: String = row.try_get("expires_at")?;
        let expires_at = DateTime::parse_from_rfc3339(&expires_at)
            .map_err(|err| DomainError::internal(format!("corrupt session expires_at: {err}")))?
            .with_timezone(&Utc);
        Ok(now < expires_at)
    }

    async fn delete_all(&self) -> Result<(), DomainError> {
        sqlx::query("DELETE FROM sessions")
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

/// Local-mode `AuthService` (UC-34 / FR-AU-04). Per-request authentication
/// presents the session id created at login — not a bearer token, per UC-34's
/// postcondition ("a session must be created to keep track of the login").
/// A session id that does not exist, or has expired, is `Unauthorized`
/// (mirrors AF-02's "wrong credentials" outcome at the per-request layer,
/// since a request carrying no valid session is exactly as unauthenticated
/// as one carrying a wrong password).
#[derive(Clone)]
pub struct LocalAuthService<SR, C> {
    sessions: SR,
    clock: C,
}

impl<SR, C> LocalAuthService<SR, C>
where
    SR: SessionRepository,
    C: Clock,
{
    pub fn new(sessions: SR, clock: C) -> Self {
        Self { sessions, clock }
    }
}

impl<SR, C> AuthService for LocalAuthService<SR, C>
where
    SR: SessionRepository,
    C: Clock,
{
    async fn authenticate(&self, token: &str) -> Result<Principal, DomainError> {
        let id = Uuid::parse_str(token.trim()).map_err(|_| DomainError::Unauthorized)?;
        if self.sessions.is_valid(id, self.clock.now()).await? {
            Ok(Principal {
                user_id: "owner".to_string(),
            })
        } else {
            Err(DomainError::Unauthorized)
        }
    }

    fn mode(&self) -> AuthMode {
        AuthMode::Local
    }
}

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
        let row = sqlx::query(
            "SELECT COUNT(*) AS remaining FROM recovery_codes WHERE consumed_at IS NULL",
        )
        .fetch_one(&self.pool)
        .await?;
        let remaining: i64 = row.try_get("remaining")?;
        Ok(u32::try_from(remaining).unwrap_or(u32::MAX))
    }
}
