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
///
/// `email_confirmed` rides along (issue #102) so a client learns the state on
/// the call it already makes, rather than having to follow every login with a
/// second round-trip to the account endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalLoginResult {
    pub success: bool,
    pub session_id: Uuid,
    pub email_confirmed: bool,
}

/// The authenticated owner's account state (issue #102 / FR-AU-13). What the
/// front-end's catalog lock reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalAccountResult {
    pub email: String,
    pub email_confirmed: bool,
}

/// The outcome of confirming the owner's address (issue #102 / FR-AU-14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmEmailResult {
    pub success: bool,
    pub email: String,
    pub email_confirmed: bool,
}

/// The outcome of resending the confirmation message (issue #102 / FR-AU-15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResendConfirmationResult {
    pub success: bool,
    pub sent: bool,
}

/// The outcome of requesting a password reset (issue #102 / FR-AU-16).
///
/// Carries nothing but `success`, and always the same value: the response must
/// not reveal whether the submitted address is the one registered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPasswordResetResult {
    pub success: bool,
}

/// The outcome of completing a password reset (issue #102 / FR-AU-16).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletePasswordResetResult {
    pub success: bool,
    pub email: String,
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
    /// Always `false` here — an address is confirmed by UC-01's confirm step,
    /// never by creating the account. Present so a client reads the same field
    /// on every auth response instead of inferring it from which call it made.
    pub email_confirmed: bool,
    /// Whether the confirmation message was actually handed to a transport
    /// (issue #102 / UC-01 AF-06). `false` today on every install: delivery is
    /// an external service that is not yet integrated. The account is created
    /// and the session is open either way — a send that failed is reported,
    /// never rolled back.
    pub confirmation_sent: bool,
    /// The reason code when `confirmation_sent` is `false`; `None` otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmation_error: Option<String>,
}

/// The single owner's local-login credential row (SRD §4.9). Singleton —
/// there is exactly one row, `id = 1`.
#[derive(Debug, Clone)]
pub struct LocalCredential {
    pub email: String,
    pub password_hash: String,
    /// When the owner proved control of `email`, or `None` if they have not
    /// (issue #102). Nothing in the core refuses an operation because this is
    /// `None` (FR-AU-13): gating here while delivery cannot work would lock
    /// every existing install out of its own catalog.
    pub email_confirmed_at: Option<DateTime<Utc>>,
}

impl LocalCredential {
    pub fn email_confirmed(&self) -> bool {
        self.email_confirmed_at.is_some()
    }
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
    /// Record that the owner proved control of the stored address (issue #102
    /// / FR-AU-14). Idempotent: confirming an already-confirmed account keeps
    /// the original timestamp, so "when was this confirmed" stays answerable.
    async fn confirm_email(&self, confirmed_at: DateTime<Utc>) -> Result<(), DomainError>;
    /// Replace the stored password after a completed reset (issue #102 /
    /// FR-AU-16). Takes no address: a reset changes the credential, never who
    /// the account belongs to, and `email_confirmed_at` is left alone for the
    /// same reason — the address did not change, so what was proved about it
    /// still holds.
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
    /// Delete every session (issue #102 / FR-AU-16).
    ///
    /// Called after a completed password reset. A reset is what an owner does
    /// when they believe someone else may hold their credentials; leaving that
    /// someone's session open would defeat the whole point of resetting.
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
        let row = sqlx::query(
            "SELECT email, password_hash, email_confirmed_at              FROM local_login_credentials WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };
        let email_confirmed_at: Option<String> = row.try_get("email_confirmed_at")?;
        let email_confirmed_at = email_confirmed_at
            .map(|value| {
                DateTime::parse_from_rfc3339(&value)
                    .map(|parsed| parsed.with_timezone(&Utc))
                    .map_err(|err| {
                        DomainError::internal(format!("corrupt email_confirmed_at: {err}"))
                    })
            })
            .transpose()?;
        Ok(Some(LocalCredential {
            email: row.try_get("email")?,
            password_hash: row.try_get("password_hash")?,
            email_confirmed_at,
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

    async fn confirm_email(&self, confirmed_at: DateTime<Utc>) -> Result<(), DomainError> {
        // `IS NULL` keeps this idempotent without a read first: a second
        // confirmation matches no row and leaves the original timestamp.
        sqlx::query(
            "UPDATE local_login_credentials SET email_confirmed_at = ?              WHERE id = 1 AND email_confirmed_at IS NULL",
        )
        .bind(confirmed_at.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
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
