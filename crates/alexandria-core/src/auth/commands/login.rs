use crate::auth::local::{
    issue_session, LocalCredentialRepository, LocalLoginResult, SessionRepository,
};
use crate::auth::password::verify_password;
use crate::catalog::clock::Clock;
use crate::config::AuthMode;
use crate::errors::DomainError;

/// UC-34 — Local login (FR-AU-01, FR-AU-04). Verifies the submitted email
/// and password against the encrypted local credential row and, on
/// success, creates a session to keep track of the login — the caller
/// presents that session's id on subsequent requests instead of a bearer
/// token (local mode has no bearer token; that is UC-36's external-JWT
/// concern).
///
/// Generic over the credential repository, session repository, and clock
/// so the decision logic is unit-tested against trait fakes, then wired
/// with the concrete Sqlite/System collaborators at runtime (services.rs).
pub struct LocalLoginHandler<CR, SR, C> {
    credentials: CR,
    sessions: SR,
    clock: C,
    mode: AuthMode,
    session_ttl_hours: u32,
}

impl<CR, SR, C> LocalLoginHandler<CR, SR, C>
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

    /// Verify `email`/`password` and, on success, create and return a new
    /// session.
    pub async fn login(
        &self,
        email: &str,
        password: &str,
    ) -> Result<LocalLoginResult, DomainError> {
        // AF-01: the active auth mode must be local login.
        if self.mode != AuthMode::Local {
            return Err(DomainError::Unauthorized);
        }

        // AF-03: local credentials must have been set (run UC-41 first).
        let credential = self
            .credentials
            .get()
            .await?
            .ok_or_else(|| DomainError::config("local credentials have not been set"))?;

        // AF-02: wrong email or password — denied without logging either.
        if credential.email != email || !verify_password(password, &credential.password_hash) {
            return Err(DomainError::Unauthorized);
        }

        let session_id = issue_session(&self.sessions, &self.clock, self.session_ttl_hours).await?;

        Ok(LocalLoginResult {
            success: true,
            session_id,
        })
    }
}
