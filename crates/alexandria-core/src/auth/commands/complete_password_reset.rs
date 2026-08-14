use crate::auth::local::{
    CompletePasswordResetResult, LocalCredentialRepository, SessionRepository,
};
use crate::auth::password::{hash_password, validate_strength};
use crate::auth::tokens::{resolve_token, AuthTokenRepository, TokenOutcome, TokenPurpose};
use crate::catalog::clock::Clock;
use crate::config::AuthMode;
use crate::errors::DomainError;

/// Reason codes a presented reset token can be refused with (FR-AU-16),
/// matching the three the confirmation code has.
pub const RESET_INVALID: &str = "reset_invalid";
pub const RESET_ALREADY_USED: &str = "reset_already_used";
pub const RESET_EXPIRED: &str = "reset_expired";

/// Complete a password reset: present the token and the new password
/// (issue #102 / FR-AU-16).
///
/// Unauthenticated — the token is the credential. It takes a confirmation
/// field for the same reason registration does: this sets the one password
/// that guards the whole catalog, and a typo here would put the owner right
/// back where they started.
///
/// Generic over the collaborators so the decision logic is unit-tested against
/// trait fakes, then wired with the concrete ones at runtime (services.rs).
pub struct CompletePasswordResetHandler<CR, SR, TR, C> {
    credentials: CR,
    sessions: SR,
    tokens: TR,
    clock: C,
    mode: AuthMode,
}

impl<CR, SR, TR, C> CompletePasswordResetHandler<CR, SR, TR, C>
where
    CR: LocalCredentialRepository,
    SR: SessionRepository,
    TR: AuthTokenRepository,
    C: Clock,
{
    pub fn new(credentials: CR, sessions: SR, tokens: TR, clock: C, mode: AuthMode) -> Self {
        Self {
            credentials,
            sessions,
            tokens,
            clock,
            mode,
        }
    }

    pub async fn complete(
        &self,
        token: &str,
        password: String,
        password_confirmation: String,
    ) -> Result<CompletePasswordResetResult, DomainError> {
        if self.mode != AuthMode::Local {
            return Err(DomainError::conflict(
                "local login is not the active auth mode",
            ));
        }

        let credential = self
            .credentials
            .get()
            .await?
            .ok_or_else(|| DomainError::config("local credentials have not been set"))?;

        let now = self.clock.now();
        // The token is checked before the password. A caller holding no valid
        // token learns nothing about the password policy by guessing at it,
        // and the work of hashing is never done on an unauthorized request —
        // this endpoint is reachable without authentication.
        let id = match resolve_token(&self.tokens, TokenPurpose::PasswordReset, token, now).await? {
            TokenOutcome::Valid { id, .. } => id,
            TokenOutcome::AlreadyUsed => return Err(refusal(RESET_ALREADY_USED)),
            TokenOutcome::Expired => return Err(refusal(RESET_EXPIRED)),
            TokenOutcome::Invalid => return Err(refusal(RESET_INVALID)),
        };

        // The same policy registration enforces (FR-AU-11), against the stored
        // address rather than a submitted one — this request carries no email,
        // and the rule "the password must not contain the address" still has
        // to hold.
        validate_strength(&password, &credential.email)?;
        if password != password_confirmation {
            return Err(DomainError::rejected(
                "password_confirmation_mismatch",
                "password confirmation does not match the password",
            ));
        }

        let password_hash = hash_password(&password)?;

        // Consume first: if this fails, nothing has changed yet and the owner
        // can try again. Doing it after the write would leave a live token for
        // a password that has already been replaced.
        if !self.tokens.consume(id, now).await? {
            // Lost a race with a concurrent use of the same token.
            return Err(refusal(RESET_ALREADY_USED));
        }
        self.credentials
            .set_password_hash(&password_hash, now)
            .await?;
        self.tokens
            .invalidate_outstanding(TokenPurpose::PasswordReset, now)
            .await?;

        // A reset is what an owner does when they think someone else may hold
        // their credentials. Leaving that someone's session open would defeat
        // it, so every session goes — including the owner's own, who logs in
        // again with the password they just set.
        self.sessions.delete_all().await?;

        Ok(CompletePasswordResetResult {
            success: true,
            email: credential.email,
        })
    }
}

/// The refusal for a token that cannot be used. As with confirmation, the
/// English message is a fallback and the code is what a client acts on.
fn refusal(code: &'static str) -> DomainError {
    let message = match code {
        RESET_ALREADY_USED => "that password reset link has already been used",
        RESET_EXPIRED => "that password reset link has expired",
        _ => "that password reset link is not valid",
    };
    DomainError::rejected(code, message)
}
