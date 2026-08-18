use chrono::Duration;

use crate::auth::commands::set_credentials::validate_email;
use crate::auth::local::{
    issue_session, LocalCredentialRepository, LocalRegisterResult, RecoveryCodeRepository,
    SessionRepository,
};
use crate::auth::mail::{MailKind, MailSender, OutboundMail};
use crate::auth::password::{hash_password, validate_strength};
use crate::auth::recovery::{generate_recovery_codes, hash_recovery_code};
use crate::auth::tokens::{
    generate_confirmation_code, hash_token, AuthTokenRepository, TokenPurpose,
};
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
pub struct RegisterLocalAccountHandler<CR, SR, TR, M, C, RR> {
    credentials: CR,
    sessions: SR,
    recovery_codes: RR,
    tokens: TR,
    mail: M,
    clock: C,
    mode: AuthMode,
    session_ttl_hours: u32,
    confirmation_ttl_hours: u32,
}

impl<CR, SR, TR, M, C, RR> RegisterLocalAccountHandler<CR, SR, TR, M, C, RR>
where
    CR: LocalCredentialRepository,
    SR: SessionRepository,
    TR: AuthTokenRepository,
    M: MailSender,
    C: Clock,
    RR: RecoveryCodeRepository,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        credentials: CR,
        sessions: SR,
        recovery_codes: RR,
        tokens: TR,
        mail: M,
        clock: C,
        mode: AuthMode,
        session_ttl_hours: u32,
        confirmation_ttl_hours: u32,
    ) -> Self {
        Self {
            credentials,
            sessions,
            recovery_codes,
            tokens,
            mail,
            clock,
            mode,
            session_ttl_hours,
            confirmation_ttl_hours,
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
        // rolled back. The two writes would need a shared transaction across
        // two repository ports, which no other command here does, and the
        // account left behind is exactly the one the caller asked for. They
        // obtain a session through UC-34.
        let session_id = issue_session(&self.sessions, &self.clock, self.session_ttl_hours).await?;

        // UC-01 AF-06: the account is created, and the core reports whether
        // the confirmation message could be sent. A send failure is never
        // fatal here — the account is the thing the caller asked for, it
        // exists, and the owner can ask for another message through resend.
        // Today this is `false` on every install: delivery is an external
        // service that is not yet integrated.
        let (confirmation_sent, confirmation_error) = self.send_confirmation(&email).await;

        Ok(LocalRegisterResult {
            success: true,
            email,
            session_id,
            email_confirmed: false,
            confirmation_sent,
            confirmation_error,
            recovery_codes,
        })
    }

    /// Mint a confirmation code, record it, and try to send it.
    ///
    /// Returns whether it went and, if not, the reason code — never an `Err`:
    /// every failure in here is reported on the result rather than failing the
    /// registration around it. A token that could not be sent is deleted
    /// again, so nothing usable is left behind for a message that never went
    /// out, and the resend interval is not already running when the owner asks
    /// for another.
    async fn send_confirmation(&self, email: &str) -> (bool, Option<String>) {
        let now = self.clock.now();
        let code = generate_confirmation_code();
        let expires_at = now + Duration::hours(i64::from(self.confirmation_ttl_hours));

        let token_id = match self
            .tokens
            .insert(
                TokenPurpose::EmailConfirmation,
                &hash_token(&code),
                email,
                now,
                expires_at,
            )
            .await
        {
            Ok(id) => id,
            Err(err) => return (false, Some(reason_code(&err))),
        };

        match self
            .mail
            .send(OutboundMail {
                to: email.to_string(),
                kind: MailKind::EmailConfirmation,
                secret: code,
            })
            .await
        {
            Ok(()) => (true, None),
            Err(err) => {
                let _ = self.tokens.delete(token_id).await;
                (false, Some(reason_code(&err)))
            }
        }
    }
}

/// The reason code to report for a failed send, or a generic one for a failure
/// that carries none. Never the underlying message: this rides on a `201`
/// response body, and a database error's text is not a caller's business.
fn reason_code(err: &DomainError) -> String {
    match err {
        DomainError::Unavailable(rejection) => rejection.code.to_string(),
        _ => "confirmation_send_failed".to_string(),
    }
}
