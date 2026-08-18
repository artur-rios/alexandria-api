use crate::auth::local::{LocalAccountResult, LocalCredentialRepository, RecoveryCodeRepository};
use crate::auth::AuthService;
use crate::errors::DomainError;

/// Report the authenticated owner's account state (FR-AU-18): the stored
/// address and how many recovery codes remain unspent.
///
/// This is the query the front-end's catalog lock reads.
///
/// Generic over the auth service and credential repository so the decision
/// logic is unit-tested against trait fakes, then wired with the concrete
/// collaborators at runtime (services.rs).
pub struct GetLocalAccountHandler<A, CR, RR> {
    auth: A,
    credentials: CR,
    recovery_codes: RR,
}

impl<A, CR, RR> GetLocalAccountHandler<A, CR, RR>
where
    A: AuthService,
    CR: LocalCredentialRepository,
    RR: RecoveryCodeRepository,
{
    pub fn new(auth: A, credentials: CR, recovery_codes: RR) -> Self {
        Self {
            auth,
            credentials,
            recovery_codes,
        }
    }

    pub async fn get(&self, token: &str) -> Result<LocalAccountResult, DomainError> {
        self.auth.authenticate(token).await?;

        let credential = self
            .credentials
            .get()
            .await?
            .ok_or_else(|| DomainError::config("local credentials have not been set"))?;

        Ok(LocalAccountResult {
            recovery_codes_remaining: self.recovery_codes.remaining().await?,
            email: credential.email,
        })
    }
}
