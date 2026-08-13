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

        Ok(match row {
            Some(row) => Some(LocalCredential {
                email: row.try_get("email")?,
                password_hash: row.try_get("password_hash")?,
            }),
            None => None,
        })
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
