use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{decode, decode_header, DecodingKey, Validation};
use serde::Deserialize;

use crate::auth::{AuthService, Principal};
use crate::config::AuthMode;
use crate::errors::DomainError;

/// Standard JWT claims this service reads. `sub` becomes the authenticated
/// principal's id; `exp` is validated by `jsonwebtoken` itself.
#[derive(Debug, Deserialize)]
struct Claims {
    sub: String,
}

/// JWKS fetch port (UC-36 / FR-AU-02). Kept separate from `ExternalAuthService`
/// so the decision logic (mode check, header/claims handling) is
/// unit-testable against a fake with no network call, and so the real
/// HTTP implementation can be swapped for whichever external auth service
/// is plugged in later without touching the handler (Testing Specification
/// §6.2).
#[allow(async_fn_in_trait)]
pub trait JwksProvider: Send + Sync {
    /// Fetch the external auth service's current signing keys. `Err` means
    /// the service could not be reached (UC-36 AF-03).
    async fn fetch(&self) -> Result<JwkSet, DomainError>;
}

/// Real JWKS provider: fetches and parses the JSON Web Key Set from the
/// configured `jwks_url` on every call. No caching yet — the external auth
/// service integration is expected to be plugged in and hardened (caching,
/// retry, key rotation) once a real provider is selected; this establishes
/// the structure UC-36 calls for.
#[derive(Clone)]
pub struct HttpJwksProvider {
    jwks_url: String,
    client: reqwest::Client,
}

impl HttpJwksProvider {
    pub fn new(jwks_url: String) -> Self {
        Self {
            jwks_url,
            client: reqwest::Client::new(),
        }
    }
}

impl JwksProvider for HttpJwksProvider {
    async fn fetch(&self) -> Result<JwkSet, DomainError> {
        let response = self
            .client
            .get(&self.jwks_url)
            .send()
            .await
            .map_err(|err| DomainError::service_unavailable(format!("jwks fetch failed: {err}")))?;

        response.json::<JwkSet>().await.map_err(|err| {
            DomainError::service_unavailable(format!("invalid jwks response: {err}"))
        })
    }
}

/// External-JWT `AuthService` (UC-36 / FR-AU-02, FR-AU-03). Validates the
/// caller's bearer JWT signature and standard claims against the external
/// auth service's published keys, fetched via `jwks`.
///
/// Generic over the JWKS provider so the decision logic is unit-tested
/// against a trait fake, then wired with the real `HttpJwksProvider` at
/// runtime (services.rs).
#[derive(Clone)]
pub struct ExternalAuthService<J> {
    jwks: J,
}

impl<J> ExternalAuthService<J>
where
    J: JwksProvider,
{
    pub fn new(jwks: J) -> Self {
        Self { jwks }
    }
}

impl<J> AuthService for ExternalAuthService<J>
where
    J: JwksProvider,
{
    async fn authenticate(&self, token: &str) -> Result<Principal, DomainError> {
        let token = token.trim();
        if token.is_empty() {
            return Err(DomainError::Unauthorized);
        }

        // AF-02: a malformed token carries no usable header/kid.
        let header = decode_header(token).map_err(|_| DomainError::Unauthorized)?;
        let kid = header.kid.as_deref().ok_or(DomainError::Unauthorized)?;

        // AF-03: the external auth service could not be reached.
        let jwks = self.jwks.fetch().await?;

        // AF-02: no matching key, or the signature/claims (including
        // expiry) do not validate.
        let jwk = jwks.find(kid).ok_or(DomainError::Unauthorized)?;
        let decoding_key = DecodingKey::from_jwk(jwk).map_err(|_| DomainError::Unauthorized)?;
        let validation = Validation::new(header.alg);
        let data = decode::<Claims>(token, &decoding_key, &validation)
            .map_err(|_| DomainError::Unauthorized)?;

        Ok(Principal {
            user_id: data.claims.sub,
        })
    }

    fn mode(&self) -> AuthMode {
        AuthMode::External
    }
}
