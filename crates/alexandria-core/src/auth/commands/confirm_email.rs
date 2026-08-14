use crate::auth::local::{ConfirmEmailResult, LocalCredentialRepository};
use crate::auth::tokens::{resolve_token, AuthTokenRepository, TokenOutcome, TokenPurpose};
use crate::catalog::clock::Clock;
use crate::config::AuthMode;
use crate::errors::DomainError;

/// Reason codes a presented confirmation code can be refused with
/// (FR-AU-14). Three distinct outcomes, because "that code is wrong", "that
/// one already worked", and "that one aged out — ask for another" call for
/// three different things from an owner.
pub const CONFIRMATION_INVALID: &str = "confirmation_invalid";
pub const CONFIRMATION_ALREADY_USED: &str = "confirmation_already_used";
pub const CONFIRMATION_EXPIRED: &str = "confirmation_expired";

/// Confirm the owner's e-mail address with the code that was sent to it
/// (issue #102 / FR-AU-14).
///
/// Takes no token and requires no session, deliberately. The code itself is
/// the proof — that is the entire mechanism — and demanding a session as well
/// would stop an owner confirming from the device that received the message,
/// which is the device they are most likely holding.
///
/// Generic over the repositories and clock so the decision logic is
/// unit-tested against trait fakes, then wired with the concrete collaborators
/// at runtime (services.rs).
pub struct ConfirmEmailHandler<CR, TR, C> {
    credentials: CR,
    tokens: TR,
    clock: C,
    mode: AuthMode,
}

impl<CR, TR, C> ConfirmEmailHandler<CR, TR, C>
where
    CR: LocalCredentialRepository,
    TR: AuthTokenRepository,
    C: Clock,
{
    pub fn new(credentials: CR, tokens: TR, clock: C, mode: AuthMode) -> Self {
        Self {
            credentials,
            tokens,
            clock,
            mode,
        }
    }

    pub async fn confirm(&self, code: &str) -> Result<ConfirmEmailResult, DomainError> {
        // The active auth mode must be local login: there is no local account
        // to confirm in external mode.
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
        let outcome =
            resolve_token(&self.tokens, TokenPurpose::EmailConfirmation, code, now).await?;

        // An already-confirmed account answers a *known* code with success
        // rather than an error. The owner's request has already come true, and
        // an app that re-sends the code on a retry (or a second tap on the
        // same button) should not be told something went wrong. An unknown
        // code is still invalid: idempotence is not a reason to accept
        // anything at all.
        if credential.email_confirmed() {
            return match outcome {
                TokenOutcome::Invalid => Err(refusal(CONFIRMATION_INVALID)),
                _ => Ok(ConfirmEmailResult {
                    success: true,
                    email: credential.email,
                    email_confirmed: true,
                }),
            };
        }

        let id = match outcome {
            TokenOutcome::Valid { id, .. } => id,
            TokenOutcome::AlreadyUsed => return Err(refusal(CONFIRMATION_ALREADY_USED)),
            TokenOutcome::Expired => return Err(refusal(CONFIRMATION_EXPIRED)),
            TokenOutcome::Invalid => return Err(refusal(CONFIRMATION_INVALID)),
        };

        // Consume first, then confirm. If the write below were to fail, the
        // code is spent and the owner asks for another — annoying but sound.
        // The other order would leave a confirmed account with a live code
        // still sitting in an inbox.
        if !self.tokens.consume(id, now).await? {
            // Lost a race with a concurrent presentation of the same code.
            return Err(refusal(CONFIRMATION_ALREADY_USED));
        }
        self.credentials.confirm_email(now).await?;
        // Every earlier code still in the inbox stops working now.
        self.tokens
            .invalidate_outstanding(TokenPurpose::EmailConfirmation, now)
            .await?;

        Ok(ConfirmEmailResult {
            success: true,
            email: credential.email,
            email_confirmed: true,
        })
    }
}

/// The refusal for a code that cannot be used, in the shape a client acts on.
///
/// The English message says the same thing for all three: which one it was
/// travels in the code, and a client renders its own sentence from that
/// (issue #101).
fn refusal(code: &'static str) -> DomainError {
    let message = match code {
        CONFIRMATION_ALREADY_USED => "that confirmation code has already been used",
        CONFIRMATION_EXPIRED => "that confirmation code has expired",
        _ => "that confirmation code is not valid",
    };
    DomainError::rejected(code, message)
}
