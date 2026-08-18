use crate::auth::local::{LocalAccountResult, LocalCredentialRepository, RecoveryCodeRepository};
use crate::auth::AuthService;
use crate::errors::DomainError;

/// Report the authenticated owner's account state (issue #102 / FR-AU-13):
/// the stored address and whether it has been confirmed.
///
/// This is the query the front-end's catalog lock reads. The core answers it
/// and does nothing else with the answer — it never refuses an operation
/// because the address is unconfirmed. Gating here while delivery is not yet
/// integrated would lock every install out of its own catalog, and the policy
/// belongs to the client anyway: the core's job is to know the truth, not to
/// decide what a product does about it.
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

        let email_confirmed = credential.email_confirmed();
        Ok(LocalAccountResult {
            recovery_codes_remaining: self.recovery_codes.remaining().await?,
            email: credential.email,
            email_confirmed,
        })
    }
}
