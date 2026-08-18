//! UC-44: replace the owner's recovery codes with a fresh set (FR-AU-17).
//!
//! Without this, recovery is finite — ten redemptions and the account is
//! unrecoverable again, which is the state recovery codes exist to escape.
//! It is also the only answer to a printed list going missing.

use crate::auth::local::{
    LocalCredentialRepository, RecoveryCodeRepository, RegenerateRecoveryCodesResult,
};
use crate::auth::recovery::{generate_recovery_codes, hash_recovery_code};
use crate::auth::AuthService;
use crate::catalog::clock::Clock;
use crate::config::AuthMode;
use crate::errors::DomainError;

pub struct RegenerateRecoveryCodesHandler<A, CR, RR, C> {
    auth: A,
    credentials: CR,
    recovery_codes: RR,
    clock: C,
    mode: AuthMode,
}

impl<A, CR, RR, C> RegenerateRecoveryCodesHandler<A, CR, RR, C>
where
    A: AuthService,
    CR: LocalCredentialRepository,
    RR: RecoveryCodeRepository,
    C: Clock,
{
    pub fn new(auth: A, credentials: CR, recovery_codes: RR, clock: C, mode: AuthMode) -> Self {
        Self {
            auth,
            credentials,
            recovery_codes,
            clock,
            mode,
        }
    }

    /// Issue a fresh set, invalidating every existing code.
    ///
    /// Authenticated, unlike redemption: this is the owner who still has
    /// access, topping up before they need it. Every old code dies, used or
    /// not — a partial refill would leave them unsure which of their written
    /// codes still work.
    pub async fn regenerate(
        &self,
        token: &str,
    ) -> Result<RegenerateRecoveryCodesResult, DomainError> {
        // AF-02: the caller must be the authenticated owner.
        self.auth.authenticate(token).await?;

        // AF-01: the active auth mode must be local login (FR-AU-03).
        if self.mode != AuthMode::Local {
            return Err(DomainError::conflict(
                "local login is not the active auth mode",
            ));
        }

        // AF-03: there must be an account to hold the codes.
        if self.credentials.get().await?.is_none() {
            return Err(DomainError::NotFound);
        }

        let recovery_codes = generate_recovery_codes();
        let hashes: Vec<String> = recovery_codes
            .iter()
            .map(|c| hash_recovery_code(c))
            .collect();
        self.recovery_codes
            .replace_all(&hashes, self.clock.now())
            .await?;

        Ok(RegenerateRecoveryCodesResult { recovery_codes })
    }
}
