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
