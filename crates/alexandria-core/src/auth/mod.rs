pub mod commands;
pub mod external;
pub mod heimdall;
pub mod local;
pub mod mail;
pub mod password;
pub mod tokens;

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
/// single owner. Kept as a minimal `AuthService` reference implementation;
/// the runtime now wires `RuntimeAuthService` (local login, UC-34/35, or
/// external JWT, UC-36) instead. Handler-level authorization (AF-02
/// "unauthorized") is still exercised via the `AuthService` trait with
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

/// The `AuthService` actually wired at runtime (services.rs), selected once
/// at startup from `AuthSettings.mode` (FR-AU-01: exactly one mode active).
/// Delegates to whichever concrete service is active; never both (FR-AU-03).
#[derive(Clone)]
pub enum RuntimeAuthService {
    Local(
        local::LocalAuthService<local::SqliteSessionRepository, crate::catalog::clock::SystemClock>,
    ),
    External(external::ExternalAuthService<external::HttpJwksProvider>),
}

impl AuthService for RuntimeAuthService {
    async fn authenticate(&self, token: &str) -> Result<Principal, DomainError> {
        match self {
            RuntimeAuthService::Local(service) => service.authenticate(token).await,
            RuntimeAuthService::External(service) => service.authenticate(token).await,
        }
    }

    fn mode(&self) -> AuthMode {
        match self {
            RuntimeAuthService::Local(_) => AuthMode::Local,
            RuntimeAuthService::External(_) => AuthMode::External,
        }
    }
}
