use chrono::Duration;

use crate::auth::local::{LocalCredentialRepository, RequestPasswordResetResult};
use crate::auth::mail::{MailKind, MailSender, OutboundMail};
use crate::auth::tokens::{generate_reset_token, hash_token, AuthTokenRepository, TokenPurpose};
use crate::catalog::clock::Clock;
use crate::config::AuthMode;
use crate::errors::DomainError;

/// Request a password reset for an address (issue #102 / FR-AU-16).
///
/// This is the operation that makes the owner's password recoverable at all.
/// Before it, registration took a password, hashed it, and offered no way
/// back: a forgotten password meant a lost catalog, and the sign-up screen had
/// to say so.
///
/// Unauthenticated by definition — it is what someone does when they cannot
/// authenticate.
///
/// Generic over the collaborators so the decision logic is unit-tested against
/// trait fakes, then wired with the concrete ones at runtime (services.rs).
pub struct RequestPasswordResetHandler<CR, TR, M, C> {
    credentials: CR,
    tokens: TR,
    mail: M,
    clock: C,
    mode: AuthMode,
    reset_ttl_minutes: u32,
}

impl<CR, TR, M, C> RequestPasswordResetHandler<CR, TR, M, C>
where
    CR: LocalCredentialRepository,
    TR: AuthTokenRepository,
    M: MailSender,
    C: Clock,
{
    pub fn new(
        credentials: CR,
        tokens: TR,
        mail: M,
        clock: C,
        mode: AuthMode,
        reset_ttl_minutes: u32,
    ) -> Self {
        Self {
            credentials,
            tokens,
            mail,
            clock,
            mode,
            reset_ttl_minutes,
        }
    }

    /// Send a reset token to `email` when it is the registered address.
    ///
    /// The outcome is the same either way. An endpoint that answered
    /// differently for a registered and an unregistered address would be an
    /// oracle telling anyone who asks whether a given person owns this
    /// library — and it is reachable without authentication, which is what
    /// makes that worth caring about on a single-owner application.
    ///
    /// A transport failure is not the same thing and is *not* hidden. Whether
    /// mail is configured is a property of the installation, not of the
    /// address, so reporting it reveals nothing — and hiding it would tell an
    /// owner their reset is on the way when nothing was sent.
    pub async fn request(&self, email: &str) -> Result<RequestPasswordResetResult, DomainError> {
        if self.mode != AuthMode::Local {
            return Err(DomainError::conflict(
                "local login is not the active auth mode",
            ));
        }

        // Asked before anything address-specific happens. If the transport
        // cannot deliver, that is true for every address and says nothing
        // about this one — but attempting the send only on the address that
        // matches would make the failure itself the answer.
        self.mail.available()?;

        let credential = self.credentials.get().await?;
        let matches = credential
            .as_ref()
            .is_some_and(|stored| stored.email.eq_ignore_ascii_case(email.trim()));

        if !matches {
            return Ok(RequestPasswordResetResult { success: true });
        }
        let credential = credential.expect("matched above");

        let now = self.clock.now();
        let expires_at = now + Duration::minutes(i64::from(self.reset_ttl_minutes));
        let token = generate_reset_token();
        let token_id = self
            .tokens
            .insert(
                TokenPurpose::PasswordReset,
                &hash_token(&token),
                &credential.email,
                now,
                expires_at,
            )
            .await?;

        // As in resend: recorded before the send so a delivered token always
        // works, and removed again if the send fails so nothing usable is left
        // behind for a message that never went out.
        if let Err(err) = self
            .mail
            .send(OutboundMail {
                to: credential.email,
                kind: MailKind::PasswordReset,
                secret: token,
            })
            .await
        {
            self.tokens.delete(token_id).await?;
            return Err(err);
        }

        Ok(RequestPasswordResetResult { success: true })
    }
}
