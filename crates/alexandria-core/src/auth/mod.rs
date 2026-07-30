use crate::config::AuthMode;
use crate::errors::DomainError;

#[derive(Debug, Clone)]
pub struct Principal {
    pub user_id: String,
}

#[allow(async_fn_in_trait)]
pub trait AuthService: Send + Sync {
    async fn authenticate(&self, token: &str) -> Result<Principal, DomainError>;

    fn mode(&self) -> AuthMode;
}

/// Bearer-token auth stub. Authenticates any non-empty bearer token as the
/// single owner. Replaced by the real local-login (UC-34) and external-JWT
/// (UC-36) services once those use cases land. Handler-level authorization
/// (AF-02 "unauthorized") is still exercised via the `AuthService` trait with
/// fakes in unit tests.
#[derive(Debug, Default, Clone, Copy)]
pub struct BearerAuthService;

impl AuthService for BearerAuthService {
    async fn authenticate(&self, token: &str) -> Result<Principal, DomainError> {
        if token.trim().is_empty() {
            return Err(DomainError::Unauthorized);
        }
        Ok(Principal {
            user_id: "owner".to_string(),
        })
    }

    fn mode(&self) -> AuthMode {
        AuthMode::External
    }
}
