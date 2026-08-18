//! UC-43: set a new password using a recovery code (FR-AU-14 … FR-AU-16).
//!
//! The one operation that changes a password without knowing the old one, so
//! it is also the one that must not be usable by accident: it consumes the
//! code, and it logs everybody out.

use chrono::{DateTime, Utc};

use crate::auth::local::{
    LocalCredentialRepository, RecoveryCodeOutcome, RecoveryCodeRepository,
    RedeemRecoveryCodeResult, SessionRepository,
};
use crate::auth::password::{hash_password, validate_strength};
use crate::auth::recovery::hash_recovery_code;
use crate::catalog::clock::Clock;
use crate::config::AuthMode;
use crate::errors::DomainError;

/// Generic over every collaborator so the decision logic is unit-tested
/// against trait fakes and wired with the concrete ones at runtime
/// (services.rs).
pub struct RedeemRecoveryCodeHandler<CR, SR, RR, C> {
    credentials: CR,
    sessions: SR,
    recovery_codes: RR,
    clock: C,
    mode: AuthMode,
}

impl<CR, SR, RR, C> RedeemRecoveryCodeHandler<CR, SR, RR, C>
where
    CR: LocalCredentialRepository,
    SR: SessionRepository,
    RR: RecoveryCodeRepository,
    C: Clock,
{
    pub fn new(
        credentials: CR,
        sessions: SR,
        recovery_codes: RR,
        clock: C,
        mode: AuthMode,
    ) -> Self {
        Self {
            credentials,
            sessions,
            recovery_codes,
            clock,
            mode,
        }
    }

    /// Replace the password using one recovery code.
    ///
    /// The order of the checks is the point. The password is validated and
    /// hashed before the code table is touched, so a typo — or a hashing
    /// failure — leaves every code intact and the owner can try the same one
    /// again (FR-AU-16). Only once the new password is fully ready to write
    /// is a code spent.
    ///
    /// Two properties of that ordering are recorded here so they are not
    /// mistaken for oversights.
    ///
    /// Hashing before consuming means every well-formed unauthenticated
    /// request costs one Argon2 hash whether or not the code is real — a CPU
    /// amplifier available to anyone who can reach the endpoint. It is
    /// nonetheless the right trade: it is precisely what stops a hashing
    /// failure from burning a code. Do not "optimize" the ordering back.
    /// `MAX_PASSWORD_LENGTH` bounds the cost of a single hash and the API
    /// binds to loopback by default (IR-03).
    ///
    /// `validate_strength(&new_password, &credential.email)` rejects a
    /// password containing the address, so an unauthenticated caller can
    /// probe the owner's e-mail local part by watching which passwords are
    /// refused. Accepted deliberately: low value against a single-user
    /// loopback service, and removing the check would weaken the password
    /// policy on the one path that sets a password without knowing the old
    /// one.
    pub async fn redeem(
        &self,
        code: String,
        new_password: String,
        password_confirmation: String,
    ) -> Result<RedeemRecoveryCodeResult, DomainError> {
        // AF-01: the active auth mode must be local login (FR-AU-03).
        if self.mode != AuthMode::Local {
            return Err(DomainError::conflict(
                "local login is not the active auth mode",
            ));
        }

        // AF-02: there must be an account to recover.
        let credential = self.credentials.get().await?.ok_or(DomainError::NotFound)?;

        // AF-03/AF-04: the new password must satisfy the policy and match its
        // confirmation. Both are pure checks over the input — no read, no
        // write — so reaching them costs nothing and failing them costs no
        // code.
        validate_strength(&new_password, &credential.email)?;
        if new_password != password_confirmation {
            return Err(DomainError::rejected(
                "password_confirmation_mismatch",
                "password confirmation does not match the password",
            ));
        }

        // Hash before spending the code: hashing can still fail here, and a
        // failure after the code table is touched would burn a code for
        // nothing (decision 6). Computed but not yet written — nothing is
        // persisted until the code is confirmed spendable.
        let password_hash = hash_password(&new_password)?;

        // AF-05/AF-06: spend the code. The two rejections are deliberately
        // distinguishable — someone working down a printed list needs to know
        // whether they mistyped or already spent it (FR-AU-15).
        let now: DateTime<Utc> = self.clock.now();
        match self
            .recovery_codes
            .consume(&hash_recovery_code(&code), now)
            .await?
        {
            RecoveryCodeOutcome::Consumed => {}
            RecoveryCodeOutcome::AlreadyUsed => {
                return Err(DomainError::rejected(
                    "recovery_code_used",
                    "that recovery code has already been used",
                ))
            }
            RecoveryCodeOutcome::Unknown => {
                return Err(DomainError::rejected(
                    "recovery_code_unknown",
                    "that recovery code is not one of this account's",
                ))
            }
        }

        // `set_password_hash`, not `upsert`: this changes the credential,
        // never who the account belongs to.
        self.credentials
            .set_password_hash(&password_hash, now)
            .await?;

        // FR-AU-14: a redemption is what an owner does when they may have lost
        // control of the account. Leaving somebody else's session open would
        // defeat the whole point.
        self.sessions.delete_all().await?;

        Ok(RedeemRecoveryCodeResult {
            success: true,
            email: credential.email,
            recovery_codes_remaining: self.recovery_codes.remaining().await?,
        })
    }
}
