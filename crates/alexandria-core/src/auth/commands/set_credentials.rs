use crate::auth::local::{LocalCredentialRepository, LocalCredentialsResult};
use crate::auth::password::{hash_password, validate_strength};
use crate::auth::AuthService;
use crate::catalog::clock::Clock;
use crate::config::AuthMode;
use crate::errors::DomainError;

/// Validate an email address as well-formed enough to store (UC-35 /
/// FR-AU-05, AF-02). Deliberately simple — a full RFC 5322 validator is out
/// of scope for a single-owner local credential; this rejects the obvious
/// non-emails (empty, no `@`, empty local/domain part, no `.` in the
/// domain) without pretending to fully validate deliverability.
pub fn validate_email(email: &str) -> Result<String, DomainError> {
    if email.is_empty() {
        return Err(DomainError::InvalidInput("email is required".into()));
    }
    if email != email.trim() {
        return Err(DomainError::InvalidInput(
            "email must not have leading or trailing whitespace".into(),
        ));
    }
    let Some((local, domain)) = email.split_once('@') else {
        return Err(DomainError::InvalidInput(
            "email must contain exactly one '@'".into(),
        ));
    };
    if local.is_empty() || domain.is_empty() {
        return Err(DomainError::InvalidInput(
            "email must have a non-empty local and domain part".into(),
        ));
    }
    if domain.contains('@') {
        return Err(DomainError::InvalidInput(
            "email must contain exactly one '@'".into(),
        ));
    }
    if !domain.contains('.') || domain.starts_with('.') || domain.ends_with('.') {
        return Err(DomainError::InvalidInput(
            "email domain must contain a '.'".into(),
        ));
    }
    Ok(email.to_string())
}

/// UC-35 — Set or change local login credentials (FR-AU-05, FR-AU-06).
/// Salts and hashes the submitted password with Argon2 and writes/updates
/// the singleton encrypted credential row. The plaintext password is never
/// stored or logged.
///
/// The caller must be authenticated as the owner: this changes existing
/// credentials, never creates them. Creating the account is UC-41
/// (`RegisterLocalAccountHandler`), which is why the conditional
/// bootstrap branch this handler used to carry is gone — every handler in
/// this codebase now authenticates unconditionally (FR-AU-07).
///
/// Generic over the auth service, credential repository, and clock so the
/// decision logic is unit-tested against trait fakes, then wired with the
/// concrete Bearer/Sqlite/System collaborators at runtime (services.rs).
pub struct SetLocalCredentialsHandler<A, R, C> {
    auth: A,
    repo: R,
    clock: C,
    mode: AuthMode,
}

impl<A, R, C> SetLocalCredentialsHandler<A, R, C>
where
    A: AuthService,
    R: LocalCredentialRepository,
    C: Clock,
{
    pub fn new(auth: A, repo: R, clock: C, mode: AuthMode) -> Self {
        Self {
            auth,
            repo,
            clock,
            mode,
        }
    }

    /// Set or change the local-login email and password.
    pub async fn set(
        &self,
        email: String,
        password: String,
        token: &str,
    ) -> Result<LocalCredentialsResult, DomainError> {
        // AF-01: the active auth mode must be local login.
        if self.mode != AuthMode::Local {
            return Err(DomainError::InvalidState);
        }

        // AF-03: the caller must be authenticated as the owner. Creating
        // the account is UC-41's job, so there is no bootstrap case left.
        self.auth.authenticate(token).await?;

        // AF-02: the email must be well-formed and the password strong.
        let email = validate_email(&email)?;
        validate_strength(&password, &email)?;

        let password_hash = hash_password(&password)?;
        let now = self.clock.now();
        self.repo.upsert(&email, &password_hash, now).await?;

        Ok(LocalCredentialsResult {
            success: true,
            email,
        })
    }
}
