use crate::auth::commands::set_credentials::validate_email;
use crate::auth::local::{
    issue_session, LocalCredentialRepository, LocalRegisterResult, RecoveryCodeRepository,
    SessionRepository,
};
use crate::auth::password::{hash_password, validate_strength};
use crate::auth::recovery::{generate_recovery_codes, hash_recovery_code};
use crate::catalog::clock::Clock;
use crate::config::AuthMode;
use crate::errors::DomainError;

/// UC-41 — Register the local account (FR-AU-10, FR-AU-11). Creates the
/// single owner's credential row when none exists, issues recovery codes for
/// it, and opens a session, so the caller is authenticated immediately.
///
/// Takes no `AuthService`: registration is unauthenticated by definition —
/// it is what a caller does when there is nothing to authenticate with
/// yet. It is safe to leave ungated precisely because it can succeed only
/// once (AF-02); every subsequent call is a conflict.
///
/// Generic over the credential repository, session repository, recovery-code
/// repository, and clock so the decision logic is unit-tested against trait
/// fakes, then wired with the concrete Sqlite/System collaborators at
/// runtime (services.rs).
pub struct RegisterLocalAccountHandler<CR, SR, C, RR> {
    credentials: CR,
    sessions: SR,
    recovery_codes: RR,
    clock: C,
    mode: AuthMode,
    session_ttl_hours: u32,
}

impl<CR, SR, C, RR> RegisterLocalAccountHandler<CR, SR, C, RR>
where
    CR: LocalCredentialRepository,
    SR: SessionRepository,
    C: Clock,
    RR: RecoveryCodeRepository,
{
    pub fn new(
        credentials: CR,
        sessions: SR,
        recovery_codes: RR,
        clock: C,
        mode: AuthMode,
        session_ttl_hours: u32,
    ) -> Self {
        Self {
            credentials,
            sessions,
            recovery_codes,
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
        // This is the fast path — it lets the common case (no account yet)
        // short-circuit before Argon2 runs, which matters because this
        // endpoint is unauthenticated. It is not the authoritative check:
        // see the `insert_if_absent` call below.
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
            return Err(DomainError::rejected(
                "password_confirmation_mismatch",
                "password confirmation does not match the password",
            ));
        }

        let password_hash = hash_password(&password)?;
        // AF-02, authoritative: the existence check above and this write are
        // two separate statements with no shared transaction, so a second
        // registration could race between them. `insert_if_absent` closes
        // that window at the storage layer — it is the atomic
        // create-if-absent operation that actually makes "succeeds only
        // once" true, not the check above.
        let created = self
            .credentials
            .insert_if_absent(&email, &password_hash, self.clock.now())
            .await?;
        if !created {
            return Err(DomainError::conflict("a local account already exists"));
        }

        // FR-AU-13: the codes are the account's only recovery path, so they
        // are written before the caller is told the account exists. Their
        // plaintext is returned here and nowhere else, ever.
        let recovery_codes = generate_recovery_codes();
        let hashes: Vec<String> = recovery_codes
            .iter()
            .map(|c| hash_recovery_code(c))
            .collect();
        self.recovery_codes
            .replace_all(&hashes, self.clock.now())
            .await?;

        // AF-06: if this fails the account still exists — deliberately not
        // rolled back. The three writes above have no shared transaction
        // across their two repository ports, and the account left behind is
        // exactly the one the caller asked for. They obtain a session
        // through UC-34.
        let session_id = issue_session(&self.sessions, &self.clock, self.session_ttl_hours).await?;

        Ok(LocalRegisterResult {
            success: true,
            email,
            session_id,
            recovery_codes,
        })
    }
}
