//! External-mode authentication against Heimdall (UC-36 / FR-AU-02, FR-AU-03).
//!
//! Heimdall signs its tokens HS256 with a secret held in its own environment,
//! publishes no key set and no introspection endpoint, and deliberately writes
//! no `kid`. Verification is therefore offline, against the same secret,
//! configured here — which means no network call on any request path, and no
//! dependency on Heimdall being reachable for Alexandria to authenticate.
//!
//! See `docs/superpowers/specs/2026-08-17-heimdall-external-auth-design.md`.

use std::collections::HashSet;

use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::{AuthService, Principal};
use crate::config::{AuthMode, AuthSettings};
use crate::errors::DomainError;

/// The claims Alexandria reads from a Heimdall token.
///
/// Every field is optional because Heimdall omits a claim rather than emitting
/// it empty when it does not apply: `IdentityUserMapper` writes `scopeId` only
/// for a `User`, `ownedScopeIds` only for a `ScopeAdmin`, and `mfaPending`
/// only on a two-factor challenge token. Each check below requires what it
/// needs.
///
/// Note the absence of `sub`: Heimdall carries its subject in `id`. `role` and
/// `scopePermissions` are written too, and deliberately not read — nothing
/// here authorizes on either.
#[derive(Debug, Deserialize)]
struct HeimdallClaims {
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "scopeId", default)]
    scope_id: Option<String>,
    #[serde(rename = "ownedScopeIds", default)]
    owned_scope_ids: Option<String>,
    #[serde(rename = "mfaPending", default)]
    mfa_pending: Option<String>,
}

/// Verifies Heimdall-issued JWTs offline and authenticates their holder as the
/// owner when they belong to the configured scope.
///
/// Holds everything the decision needs, resolved once at startup. There is no
/// type parameter and no collaborator because there is nothing to call: the
/// whole decision is a function of the token and this configuration, which is
/// what makes it testable without a double.
#[derive(Clone)]
pub struct HeimdallAuthService {
    /// The keys a signature may verify against: the current secret, then the
    /// previous one while a rotation is in flight. Empty when unconfigured,
    /// in which case nothing verifies.
    keys: Vec<DecodingKey>,
    /// The scope a token must name. `None` when unconfigured or unparseable,
    /// in which case no token is accepted.
    scope_id: Option<Uuid>,
    issuer: Option<String>,
    audience: Option<String>,
}

impl HeimdallAuthService {
    /// Build from startup configuration.
    ///
    /// Infallible by design: `AuthSettings::validate` is what refuses to start
    /// a misconfigured process, and it runs first. Anything that reaches here
    /// with an unusable configuration anyway — a test constructing
    /// `Settings::default()`, say — gets a service with no key and no scope,
    /// which refuses every token. Defence in depth; never a silent pass.
    pub fn from_settings(settings: &AuthSettings) -> Self {
        let mut keys = Vec::new();
        let current = settings.heimdall_token_secret.expose().trim();
        if !current.is_empty() {
            keys.push(DecodingKey::from_secret(current.as_bytes()));
        }
        let previous = settings.heimdall_token_secret_previous.expose().trim();
        // Heimdall ignores a previous secret equal to the current one, and so
        // does this: the same value under two names is not a rotation.
        if !previous.is_empty() && previous != current {
            keys.push(DecodingKey::from_secret(previous.as_bytes()));
        }

        Self {
            keys,
            scope_id: Uuid::parse_str(settings.heimdall_scope_id.trim()).ok(),
            issuer: non_empty(&settings.heimdall_issuer),
            audience: non_empty(&settings.heimdall_audience),
        }
    }

    /// The validation rules, built per call because `Validation` is not
    /// `Clone` and building it is a handful of field writes.
    ///
    /// Every field is set explicitly rather than left to the library's
    /// defaults. `validate_nbf` in particular is *off* by default, and the
    /// audience check has to be disabled outright when none is configured —
    /// silent defaults are the wrong thing to inherit in the one function
    /// standing between a caller and the whole catalog.
    ///
    /// `jsonwebtoken` checks `iss`/`aud` only when the token actually carries
    /// the claim — configuring one without adding it to
    /// `required_spec_claims` would let a token that omits it slip past
    /// unchecked, which defeats the point of configuring it at all. So each
    /// claim is required exactly when the matching setting is configured.
    fn validation(&self) -> Validation {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.validate_exp = true;
        validation.validate_nbf = true;
        // `exp` is always required. `iss`/`aud` join it only when configured,
        // so an unconfigured install keeps accepting Heimdall's default
        // tokens, which carry neither claim.
        let mut required = HashSet::from(["exp".to_string()]);

        // `Validation::new` starts with no issuer, so the unconfigured arm has
        // nothing to undo — unlike the audience, which defaults to being
        // validated.
        if let Some(issuer) = &self.issuer {
            validation.set_issuer(&[issuer]);
            required.insert("iss".to_string());
        }
        match &self.audience {
            Some(audience) => {
                validation.set_audience(&[audience]);
                required.insert("aud".to_string());
            }
            None => validation.validate_aud = false,
        }
        validation.required_spec_claims = required;

        validation
    }

    /// Whether the token names the configured scope — as the scope its holder
    /// belongs to (`scopeId`, how a `User` carries it) or as one of the scopes
    /// they own (`ownedScopeIds`, comma-separated, how a `ScopeAdmin` carries
    /// them).
    ///
    /// A Heimdall `SystemAdmin` carries neither and is refused: administering
    /// the identity provider is not owning this library, and FR-AU-07
    /// authorizes one owner over every catalog operation.
    ///
    /// Both sides are compared as parsed UUIDs, so the configured value and
    /// the claim cannot disagree over letter case or formatting.
    fn names_configured_scope(&self, claims: &HeimdallClaims) -> bool {
        let Some(expected) = self.scope_id else {
            return false;
        };

        let belongs_to = claims
            .scope_id
            .as_deref()
            .and_then(|value| Uuid::parse_str(value.trim()).ok())
            == Some(expected);

        let owns = claims
            .owned_scope_ids
            .as_deref()
            .unwrap_or_default()
            .split(',')
            .filter_map(|value| Uuid::parse_str(value.trim()).ok())
            .any(|value| value == expected);

        belongs_to || owns
    }
}

/// A configured string, or `None` when the key is unset. Blank is unset.
fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

impl AuthService for HeimdallAuthService {
    /// Every refusal is the same `Unauthorized`, with no code and no detail.
    /// Naming which check failed would tell a caller which knob to turn next;
    /// Heimdall answers its own login failures identically for that reason.
    async fn authenticate(&self, token: &str) -> Result<Principal, DomainError> {
        let token = token.trim();
        if token.is_empty() {
            return Err(DomainError::Unauthorized);
        }

        // The algorithm comes from configuration, never from the token's own
        // header. Trusting the header is the JWT algorithm-confusion attack:
        // `none` asks to skip verification, and an asymmetric `alg` invites a
        // public key to be accepted as an HMAC secret. Heimdall writes no
        // `kid`, so there is no key selection to attack on top of that.
        let header = decode_header(token).map_err(|_| DomainError::Unauthorized)?;
        if header.alg != Algorithm::HS256 {
            return Err(DomainError::Unauthorized);
        }

        // Each configured key in turn: the current secret, then the previous
        // one while Heimdall is mid-rotation. No key configured means nothing
        // verifies and every token is refused.
        let validation = self.validation();
        let claims = self
            .keys
            .iter()
            .find_map(|key| decode::<HeimdallClaims>(token, key, &validation).ok())
            .map(|data| data.claims)
            .ok_or(DomainError::Unauthorized)?;

        // Only now are claims read for a decision: nothing below this line
        // acts on a token that has not been shown to be authentic.

        // A two-factor challenge token is not proof of authentication.
        // Heimdall issues one *instead of* an authentication token when 2FA is
        // pending, redeemable only at its own `2fa/verify` endpoint, and keeps
        // it away from its own endpoints with a global filter. Alexandria
        // makes the check explicitly rather than relying on the challenge
        // token happening to carry no scope claim today.
        if claims.mfa_pending.as_deref() == Some("true") {
            return Err(DomainError::Unauthorized);
        }

        if !self.names_configured_scope(&claims) {
            return Err(DomainError::Unauthorized);
        }

        let user_id = claims.id.unwrap_or_default();
        if user_id.trim().is_empty() {
            return Err(DomainError::Unauthorized);
        }

        Ok(Principal { user_id })
    }

    fn mode(&self) -> AuthMode {
        AuthMode::External
    }
}

#[cfg(test)]
mod tests {
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    use serde_json::json;

    use super::*;
    use crate::config::Secret;

    const SECRET: &str = "heimdall-signing-secret";
    const OTHER_SECRET: &str = "some-other-secret";
    const SCOPE: &str = "0b8d3a6e-4a1f-4c2b-9f1e-7c5d2a9b3e40";
    const OTHER_SCOPE: &str = "9f2c7b51-3e64-4d80-a1c9-6b0f8e2d4715";
    const PERSON: &str = "3c9a1d7f-5b28-4e63-90ab-1d4f6c8e2057";

    /// Seconds since the Unix epoch, offset by `delta`. Tokens carry absolute
    /// times, so every test builds its `exp`/`nbf` from the real clock.
    fn epoch(delta: i64) -> i64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        now + delta
    }

    /// Settings for a fully configured external-mode install, with no issuer
    /// or audience — the default Heimdall shape.
    fn settings() -> AuthSettings {
        AuthSettings {
            mode: AuthMode::External,
            heimdall_token_secret: Secret::new(SECRET),
            heimdall_scope_id: SCOPE.to_string(),
            ..AuthSettings::default()
        }
    }

    /// A token signed with `secret`, carrying `claims` merged over the
    /// defaults of a valid `User` token from the configured scope.
    fn token_with(secret: &str, alg: Algorithm, claims: serde_json::Value) -> String {
        let mut body = json!({
            "id": PERSON,
            "role": "3",
            "scopeId": SCOPE,
            "exp": epoch(3600),
            "nbf": epoch(-60),
        });
        let map = body.as_object_mut().unwrap();
        for (key, value) in claims.as_object().unwrap() {
            if value.is_null() {
                map.remove(key);
            } else {
                map.insert(key.clone(), value.clone());
            }
        }
        encode(
            &Header::new(alg),
            &body,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap()
    }

    /// The common case: correctly signed, unexpired, from the configured scope.
    fn valid_token() -> String {
        token_with(SECRET, Algorithm::HS256, json!({}))
    }

    #[tokio::test]
    async fn given_valid_token_from_configured_scope_when_authenticated_then_principal_is_the_id_claim(
    ) {
        let service = HeimdallAuthService::from_settings(&settings());

        let principal = service.authenticate(&valid_token()).await.unwrap();

        assert_eq!(principal.user_id, PERSON);
    }

    /// A ScopeAdmin carries no `scopeId`; the scopes they own arrive
    /// comma-separated in `ownedScopeIds`.
    #[tokio::test]
    async fn given_configured_scope_among_owned_scopes_when_authenticated_then_accepted() {
        let service = HeimdallAuthService::from_settings(&settings());
        let token = token_with(
            SECRET,
            Algorithm::HS256,
            json!({ "scopeId": null, "ownedScopeIds": format!("{OTHER_SCOPE},{SCOPE}") }),
        );

        assert!(service.authenticate(&token).await.is_ok());
    }

    /// Heimdall accepts a token signed with either configured secret while a
    /// rotation is in flight, and so must Alexandria.
    #[tokio::test]
    async fn given_token_signed_with_configured_previous_secret_when_authenticated_then_accepted() {
        let service = HeimdallAuthService::from_settings(&AuthSettings {
            heimdall_token_secret_previous: Secret::new(OTHER_SECRET),
            ..settings()
        });
        let token = token_with(OTHER_SECRET, Algorithm::HS256, json!({}));

        assert!(service.authenticate(&token).await.is_ok());
    }

    #[tokio::test]
    async fn given_token_signed_with_unconfigured_secret_when_authenticated_then_unauthorized() {
        let service = HeimdallAuthService::from_settings(&settings());
        let token = token_with(OTHER_SECRET, Algorithm::HS256, json!({}));

        assert!(matches!(
            service.authenticate(&token).await,
            Err(DomainError::Unauthorized)
        ));
    }

    /// A previous secret equal to the current one is not a rotation, and must
    /// not widen what is accepted.
    #[tokio::test]
    async fn given_previous_secret_equal_to_current_when_authenticated_then_still_only_that_secret()
    {
        let service = HeimdallAuthService::from_settings(&AuthSettings {
            heimdall_token_secret_previous: Secret::new(SECRET),
            ..settings()
        });

        assert!(service.authenticate(&valid_token()).await.is_ok());
        let foreign = token_with(OTHER_SECRET, Algorithm::HS256, json!({}));
        assert!(matches!(
            service.authenticate(&foreign).await,
            Err(DomainError::Unauthorized)
        ));
    }

    #[tokio::test]
    async fn given_expired_token_when_authenticated_then_unauthorized() {
        let service = HeimdallAuthService::from_settings(&settings());
        let token = token_with(SECRET, Algorithm::HS256, json!({ "exp": epoch(-3600) }));

        assert!(matches!(
            service.authenticate(&token).await,
            Err(DomainError::Unauthorized)
        ));
    }

    /// `jsonwebtoken` leaves `nbf` unchecked unless asked, so this pins that
    /// the service asks.
    #[tokio::test]
    async fn given_token_not_yet_valid_when_authenticated_then_unauthorized() {
        let service = HeimdallAuthService::from_settings(&settings());
        let token = token_with(SECRET, Algorithm::HS256, json!({ "nbf": epoch(3600) }));

        assert!(matches!(
            service.authenticate(&token).await,
            Err(DomainError::Unauthorized)
        ));
    }

    /// Algorithm confusion: the header must not choose the algorithm. This
    /// token is signed with the right secret and is refused solely because its
    /// header names something other than HS256.
    #[tokio::test]
    async fn given_token_signed_with_a_different_hmac_algorithm_when_authenticated_then_unauthorized(
    ) {
        let service = HeimdallAuthService::from_settings(&settings());
        let token = token_with(SECRET, Algorithm::HS512, json!({}));

        assert!(matches!(
            service.authenticate(&token).await,
            Err(DomainError::Unauthorized)
        ));
    }

    /// `alg: none` asks for verification to be skipped altogether. Assembled
    /// by hand because `jsonwebtoken` will not produce one.
    #[tokio::test]
    async fn given_unsigned_token_when_authenticated_then_unauthorized() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;

        let service = HeimdallAuthService::from_settings(&settings());
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(
            json!({ "id": PERSON, "scopeId": SCOPE, "exp": epoch(3600) }).to_string(),
        );
        let token = format!("{header}.{payload}.");

        assert!(matches!(
            service.authenticate(&token).await,
            Err(DomainError::Unauthorized)
        ));
    }

    /// A two-factor challenge token proves one factor of two. Heimdall returns
    /// it *instead of* an authentication token, and it must not open the
    /// catalog. Given every other claim of a valid token here, so the refusal
    /// can only come from the `mfaPending` check.
    #[tokio::test]
    async fn given_two_factor_challenge_token_when_authenticated_then_unauthorized() {
        let service = HeimdallAuthService::from_settings(&settings());
        let token = token_with(SECRET, Algorithm::HS256, json!({ "mfaPending": "true" }));

        assert!(matches!(
            service.authenticate(&token).await,
            Err(DomainError::Unauthorized)
        ));
    }

    #[tokio::test]
    async fn given_token_from_a_different_scope_when_authenticated_then_unauthorized() {
        let service = HeimdallAuthService::from_settings(&settings());
        let token = token_with(SECRET, Algorithm::HS256, json!({ "scopeId": OTHER_SCOPE }));

        assert!(matches!(
            service.authenticate(&token).await,
            Err(DomainError::Unauthorized)
        ));
    }

    /// A Heimdall SystemAdmin belongs to no scope and owns none, so carries
    /// neither claim. Running the identity provider is not owning this
    /// library (FR-AU-07).
    #[tokio::test]
    async fn given_token_with_no_scope_claims_when_authenticated_then_unauthorized() {
        let service = HeimdallAuthService::from_settings(&settings());
        let token = token_with(SECRET, Algorithm::HS256, json!({ "scopeId": null }));

        assert!(matches!(
            service.authenticate(&token).await,
            Err(DomainError::Unauthorized)
        ));
    }

    #[tokio::test]
    async fn given_token_without_an_id_claim_when_authenticated_then_unauthorized() {
        let service = HeimdallAuthService::from_settings(&settings());
        let token = token_with(SECRET, Algorithm::HS256, json!({ "id": null }));

        assert!(matches!(
            service.authenticate(&token).await,
            Err(DomainError::Unauthorized)
        ));
    }

    #[tokio::test]
    async fn given_empty_or_malformed_token_when_authenticated_then_unauthorized() {
        let service = HeimdallAuthService::from_settings(&settings());

        for token in ["", "   ", "not-a-jwt", "a.b.c"] {
            assert!(
                matches!(
                    service.authenticate(token).await,
                    Err(DomainError::Unauthorized)
                ),
                "{token:?} was not refused"
            );
        }
    }

    /// A misconfigured service that somehow got past `AuthSettings::validate`
    /// refuses everything rather than passing anything.
    #[tokio::test]
    async fn given_unconfigured_service_when_authenticated_then_unauthorized() {
        let service = HeimdallAuthService::from_settings(&AuthSettings::default());

        assert!(matches!(
            service.authenticate(&valid_token()).await,
            Err(DomainError::Unauthorized)
        ));
    }

    #[tokio::test]
    async fn given_configured_issuer_and_audience_when_token_matches_then_accepted() {
        let service = HeimdallAuthService::from_settings(&AuthSettings {
            heimdall_issuer: "heimdall".to_string(),
            heimdall_audience: "alexandria".to_string(),
            ..settings()
        });
        let token = token_with(
            SECRET,
            Algorithm::HS256,
            json!({ "iss": "heimdall", "aud": "alexandria" }),
        );

        assert!(service.authenticate(&token).await.is_ok());
    }

    #[tokio::test]
    async fn given_configured_issuer_when_token_names_another_then_unauthorized() {
        let service = HeimdallAuthService::from_settings(&AuthSettings {
            heimdall_issuer: "heimdall".to_string(),
            ..settings()
        });
        let token = token_with(SECRET, Algorithm::HS256, json!({ "iss": "somewhere-else" }));

        assert!(matches!(
            service.authenticate(&token).await,
            Err(DomainError::Unauthorized)
        ));
    }

    /// Heimdall signs tokens with no `iss`/`aud` when its own variables are
    /// unset, which is the default. Those tokens must be accepted.
    #[tokio::test]
    async fn given_no_configured_issuer_when_token_carries_none_then_accepted() {
        let service = HeimdallAuthService::from_settings(&settings());

        assert!(service.authenticate(&valid_token()).await.is_ok());
    }

    /// `jsonwebtoken` checks `iss` only when the claim is present, so a
    /// configured issuer alone does not make the claim required. An operator
    /// configuring an issuer to fence off a second deployment sharing the
    /// secret must have a token carrying none of it refused.
    #[tokio::test]
    async fn given_configured_issuer_when_token_carries_none_then_unauthorized() {
        let service = HeimdallAuthService::from_settings(&AuthSettings {
            heimdall_issuer: "heimdall".to_string(),
            ..settings()
        });

        assert!(matches!(
            service.authenticate(&valid_token()).await,
            Err(DomainError::Unauthorized)
        ));
    }

    #[tokio::test]
    async fn given_configured_audience_when_token_names_another_then_unauthorized() {
        let service = HeimdallAuthService::from_settings(&AuthSettings {
            heimdall_audience: "alexandria".to_string(),
            ..settings()
        });
        let token = token_with(SECRET, Algorithm::HS256, json!({ "aud": "somewhere-else" }));

        assert!(matches!(
            service.authenticate(&token).await,
            Err(DomainError::Unauthorized)
        ));
    }

    /// Same reasoning as the issuer case: a configured audience must be
    /// enforced even against a token that omits the claim entirely.
    #[tokio::test]
    async fn given_configured_audience_when_token_carries_none_then_unauthorized() {
        let service = HeimdallAuthService::from_settings(&AuthSettings {
            heimdall_audience: "alexandria".to_string(),
            ..settings()
        });

        assert!(matches!(
            service.authenticate(&valid_token()).await,
            Err(DomainError::Unauthorized)
        ));
    }

    #[tokio::test]
    async fn given_service_when_mode_queried_then_external() {
        assert_eq!(
            HeimdallAuthService::from_settings(&settings()).mode(),
            AuthMode::External
        );
    }
}
