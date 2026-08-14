use chrono::Duration;

use crate::auth::local::{LocalCredentialRepository, ResendConfirmationResult};
use crate::auth::mail::{MailKind, MailSender, OutboundMail};
use crate::auth::tokens::{
    generate_confirmation_code, hash_token, AuthTokenRepository, TokenPurpose,
};
use crate::auth::AuthService;
use crate::catalog::clock::Clock;
use crate::config::AuthMode;
use crate::errors::DomainError;

/// The refusal when a resend comes before the interval has elapsed
/// (FR-AU-15). Carries `retryAfterSeconds`, so a client shows a countdown
/// instead of an error.
pub const RESEND_TOO_SOON: &str = "resend_too_soon";

/// Send a fresh confirmation message to the owner's address (issue #102 /
/// FR-AU-15).
///
/// Requires authentication. It takes no address — it sends to whichever one is
/// stored — so it needs an authenticated caller to have a subject at all, and
/// that also keeps it from being an open mail relay pointed at an address
/// someone else chose.
///
/// Generic over the collaborators so the decision logic is unit-tested against
/// trait fakes, then wired with the concrete ones at runtime (services.rs).
pub struct ResendConfirmationHandler<A, CR, TR, M, C> {
    auth: A,
    credentials: CR,
    tokens: TR,
    mail: M,
    clock: C,
    mode: AuthMode,
    confirmation_ttl_hours: u32,
    resend_interval_seconds: u32,
}

impl<A, CR, TR, M, C> ResendConfirmationHandler<A, CR, TR, M, C>
where
    A: AuthService,
    CR: LocalCredentialRepository,
    TR: AuthTokenRepository,
    M: MailSender,
    C: Clock,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        auth: A,
        credentials: CR,
        tokens: TR,
        mail: M,
        clock: C,
        mode: AuthMode,
        confirmation_ttl_hours: u32,
        resend_interval_seconds: u32,
    ) -> Self {
        Self {
            auth,
            credentials,
            tokens,
            mail,
            clock,
            mode,
            confirmation_ttl_hours,
            resend_interval_seconds,
        }
    }

    pub async fn resend(&self, token: &str) -> Result<ResendConfirmationResult, DomainError> {
        if self.mode != AuthMode::Local {
            return Err(DomainError::conflict(
                "local login is not the active auth mode",
            ));
        }

        self.auth.authenticate(token).await?;

        let credential = self
            .credentials
            .get()
            .await?
            .ok_or_else(|| DomainError::config("local credentials have not been set"))?;

        // Nothing to confirm. A conflict rather than a silent success: an app
        // that keeps offering "resend" on a confirmed account has a bug, and
        // hiding it behind a `200` would keep it hidden.
        if credential.email_confirmed() {
            return Err(DomainError::conflict(
                "the email address is already confirmed",
            ));
        }

        let now = self.clock.now();
        let interval = Duration::seconds(i64::from(self.resend_interval_seconds));
        if let Some(last_sent) = self
            .tokens
            .last_created_at(TokenPurpose::EmailConfirmation)
            .await?
        {
            let ready_at = last_sent + interval;
            if now < ready_at {
                // Rounded up, not truncated: a client that waits the number it
                // was given must find the interval elapsed when it retries, and
                // truncating would hand back a wait that is a fraction short.
                // Ceiling rather than `+ 1`, so a whole number of seconds
                // reports itself rather than one more than itself.
                let remaining = (ready_at - now).num_milliseconds().div_euclid(1000)
                    + i64::from((ready_at - now).num_milliseconds() % 1000 != 0);
                return Err(DomainError::too_many_requests(
                    RESEND_TOO_SOON,
                    "a confirmation message was sent too recently",
                )
                .with_param("retryAfterSeconds", remaining.to_string()));
            }
        }

        // Minted and stored before the send, so a message that does go out is
        // never one whose code was never recorded. The reverse order would
        // deliver a code that cannot work.
        let code = generate_confirmation_code();
        let expires_at = now + Duration::hours(i64::from(self.confirmation_ttl_hours));
        let token_id = self
            .tokens
            .insert(
                TokenPurpose::EmailConfirmation,
                &hash_token(&code),
                &credential.email,
                now,
                expires_at,
            )
            .await?;

        // A send failure propagates. Unlike registration — where the account
        // is the thing the caller asked for and survives — sending *is* what
        // this operation does, so a failure to send is a failure of the
        // operation. Today that is every call: delivery is not yet integrated,
        // and the caller is told exactly that (`mail_not_configured`).
        //
        // The token is dropped first. It was recorded before the send so that
        // a delivered code is never one that was left unrecorded; a code that
        // was never delivered has to leave no trace, or the next resend would
        // answer `resend_too_soon` and hide the real reason behind a wait that
        // protects nothing.
        if let Err(err) = self
            .mail
            .send(OutboundMail {
                to: credential.email,
                kind: MailKind::EmailConfirmation,
                secret: code,
            })
            .await
        {
            self.tokens.delete(token_id).await?;
            return Err(err);
        }

        Ok(ResendConfirmationResult {
            success: true,
            sent: true,
        })
    }
}
