//! External-mode authentication against Heimdall (UC-36 / FR-AU-02, FR-AU-03).
//!
//! Heimdall signs its tokens ES256 and publishes the public keys at
//! `/.well-known/jwks.json`, naming the signing key in each token's `kid`.
//! Alexandria verifies against that key set (`auth::jwks`), fetched and cached
//! rather than asked for per request: a cached key verifies with no network
//! call, and Heimdall being briefly unreachable does not stop authentication.
//!
//! Heimdall used to sign HS256 with a shared secret, and accepts tokens signed
//! that way for an overlap after it moves. Alexandria does the same while the
//! secret is still configured here, and refuses them once it is removed — no
//! code change, only configuration.
//!
//! See `docs/superpowers/specs/2026-08-17-heimdall-external-auth-design.md`.

use std::collections::HashSet;
use std::sync::Arc;

use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::jwks::JwksCache;
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

/// Verifies Heimdall-issued JWTs and authenticates their holder as the owner
/// when they belong to the configured scope.
///
/// Holds everything the decision needs, resolved once at startup, apart from
/// Heimdall's public keys, which the key set cache fetches when it needs to.
#[derive(Clone)]
pub struct HeimdallAuthService {
    /// Heimdall's ES256 keys. `None` when no key set URL is configured, in
    /// which case no ES256 token verifies.
    key_set: Option<Arc<JwksCache>>,
    /// The HS256 secrets of the overlap: the current one, then the previous
    /// one while a rotation was in flight. Empty once they are removed from
    /// the configuration, in which case no HS256 token verifies.
    secrets: Vec<DecodingKey>,
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
        let url = settings.heimdall_jwks_url.trim();
        let key_set = (!url.is_empty()).then(|| JwksCache::over_http(url));
        Self::with_key_set(settings, key_set)
    }

    /// As [`HeimdallAuthService::from_settings`], with the key set given
    /// rather than fetched from the configured URL — how the tests supply
    /// Heimdall's keys.
    pub(crate) fn with_key_set(settings: &AuthSettings, key_set: Option<JwksCache>) -> Self {
        let mut secrets = Vec::new();
        let current = settings.heimdall_token_secret.expose().trim();
        if !current.is_empty() {
            secrets.push(DecodingKey::from_secret(current.as_bytes()));
        }
        let previous = settings.heimdall_token_secret_previous.expose().trim();
        // Heimdall ignores a previous secret equal to the current one, and so
        // does this: the same value under two names is not a rotation.
        if !previous.is_empty() && previous != current {
            secrets.push(DecodingKey::from_secret(previous.as_bytes()));
        }

        Self {
            key_set: key_set.map(Arc::new),
            secrets,
            scope_id: Uuid::parse_str(settings.heimdall_scope_id.trim()).ok(),
            issuer: non_empty(&settings.heimdall_issuer),
            audience: non_empty(&settings.heimdall_audience),
        }
    }

    /// The validation rules for one algorithm, built per call because
    /// `Validation` is not `Clone` and building it is a handful of field
    /// writes.
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
    fn validation(&self, algorithm: Algorithm) -> Validation {
        let mut validation = Validation::new(algorithm);
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

/// Whether the token is a two-factor challenge token rather than proof of a
/// completed authentication.
///
/// Absent claim means no challenge is pending — Heimdall's authentication
/// tokens carry no `mfaPending` at all, and rejecting them would refuse every
/// legitimate token.
///
/// Present, the value is read *permissively about its shape and strictly about
/// its meaning*: it is trimmed and lowercased, and only a value positively
/// recognised as "not pending" clears the check. Anything else — `"True"`,
/// `"yes"`, a value that has not been invented yet — counts as pending.
///
/// The asymmetry is deliberate. This literal is produced by a .NET codebase in
/// another repository, where `bool.ToString()` yields `"True"`; matching
/// `"true"` exactly would turn a refactor there into a silent open door on the
/// one check standing between a caller who has proved one factor of two and
/// the entire catalog. Failing closed costs, at worst, a `401` an operator can
/// see and diagnose.
fn is_challenge_token(value: Option<&str>) -> bool {
    match value {
        None => false,
        Some(value) => !matches!(value.trim().to_ascii_lowercase().as_str(), "false" | "0"),
    }
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

        // The header picks which of two fixed paths a token takes, and each
        // path pins both its algorithm and where its keys come from: an ES256
        // token verifies only against Heimdall's published EC keys, an HS256
        // one only against a configured secret. So the header can never choose
        // the key type — the JWT algorithm-confusion attack, in which a public
        // key is accepted as an HMAC secret, has no key to work with. Any
        // other algorithm, `none` included, is refused outright.
        let header = decode_header(token).map_err(|_| DomainError::Unauthorized)?;
        let claims = match header.alg {
            Algorithm::ES256 => {
                let key_set = self.key_set.as_ref().ok_or(DomainError::Unauthorized)?;
                let kid = header.kid.ok_or(DomainError::Unauthorized)?;
                let key = key_set.key(&kid).await.ok_or(DomainError::Unauthorized)?;
                decode::<HeimdallClaims>(token, &key, &self.validation(Algorithm::ES256))
                    .map_err(|_| DomainError::Unauthorized)?
                    .claims
            }
            // The overlap: each configured secret in turn, the current one
            // first. None configured means no HS256 token verifies.
            Algorithm::HS256 => {
                let validation = self.validation(Algorithm::HS256);
                self.secrets
                    .iter()
                    .find_map(|key| decode::<HeimdallClaims>(token, key, &validation).ok())
                    .map(|data| data.claims)
                    .ok_or(DomainError::Unauthorized)?
            }
            _ => return Err(DomainError::Unauthorized),
        };

        // Only now are claims read for a decision: nothing below this line
        // acts on a token that has not been shown to be authentic.

        // A two-factor challenge token is not proof of authentication.
        // Heimdall issues one *instead of* an authentication token when 2FA is
        // pending, redeemable only at its own `2fa/verify` endpoint, and keeps
        // it away from its own endpoints with a global filter. Alexandria
        // makes the check explicitly rather than relying on the challenge
        // token happening to carry no scope claim today. The claim's value is
        // read fail-closed; see `is_challenge_token`.
        if is_challenge_token(claims.mfa_pending.as_deref()) {
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

    /// The claims of a valid `User` token from the configured scope, with
    /// `claims` merged over them; a `null` removes the claim.
    fn body_with(claims: serde_json::Value) -> serde_json::Value {
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
        body
    }

    /// A token signed with `secret`, carrying `claims` merged over the
    /// defaults of a valid `User` token from the configured scope.
    fn token_with(secret: &str, alg: Algorithm, claims: serde_json::Value) -> String {
        let body = body_with(claims);
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
        let payload = URL_SAFE_NO_PAD
            .encode(json!({ "id": PERSON, "scopeId": SCOPE, "exp": epoch(3600) }).to_string());
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

    /// The literal is written by a .NET codebase in another repository, where
    /// `bool.ToString()` yields `"True"`. An exact match on `"true"` would let
    /// a challenge token straight through the moment that changes, so every
    /// spelling that is not positively "not pending" is refused.
    #[tokio::test]
    async fn given_challenge_token_with_an_unrecognised_pending_value_then_unauthorized() {
        let service = HeimdallAuthService::from_settings(&settings());

        for value in ["True", "TRUE", " true ", "yes", "1", "pending", ""] {
            let token = token_with(SECRET, Algorithm::HS256, json!({ "mfaPending": value }));
            assert!(
                matches!(
                    service.authenticate(&token).await,
                    Err(DomainError::Unauthorized)
                ),
                "mfaPending={value:?} was not refused"
            );
        }
    }

    /// The other side of the fail-closed rule: a claim that positively says no
    /// challenge is pending must not refuse an otherwise valid token, however
    /// it is cased.
    #[tokio::test]
    async fn given_token_whose_pending_claim_says_not_pending_then_accepted() {
        let service = HeimdallAuthService::from_settings(&settings());

        for value in ["false", "False", " FALSE ", "0"] {
            let token = token_with(SECRET, Algorithm::HS256, json!({ "mfaPending": value }));
            assert!(
                service.authenticate(&token).await.is_ok(),
                "mfaPending={value:?} was not accepted"
            );
        }
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

    // -----------------------------------------------------------------------
    // ES256, verified through Heimdall's published key set.
    // -----------------------------------------------------------------------

    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;
    use std::time::{Duration, Instant};

    use crate::auth::jwks::JwksSource;

    /// Two P-256 keys made for these tests and nothing else, with the public
    /// halves Heimdall would publish for them.
    const KEY_A_PEM: &str = include_str!("testdata/es256_a.pem");
    const KEY_B_PEM: &str = include_str!("testdata/es256_b.pem");
    const KEY_A_JWK: &str = r#"{"kty":"EC","crv":"P-256","x":"xNYqHxZs5guFNq5tp_QAZDmyzd-rfqKOFuiH_X5XUOc","y":"Kj34xYASE06sFgLfB1lUbvGKeM7Z-p7ktA82965vYdU","kid":"key-a","use":"sig","alg":"ES256"}"#;
    const KEY_B_JWK: &str = r#"{"kty":"EC","crv":"P-256","x":"F4-xfRljkHlfypeJn4cR7z4fl6A1T1uNyDKPEnzsqj8","y":"KSeCGuR2MlWqBMa91gYSGF6mfmEJJuONSca67gKWyV8","kid":"key-b","use":"sig","alg":"ES256"}"#;

    /// Heimdall's key set endpoint: answers whatever set it currently
    /// publishes, and counts how often it was asked.
    struct Heimdall {
        published: StdMutex<String>,
        fetches: AtomicUsize,
    }

    impl Heimdall {
        fn publishing(keys: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                published: StdMutex::new(key_set(keys)),
                fetches: AtomicUsize::new(0),
            })
        }

        fn publish(&self, keys: &[&str]) {
            *self.published.lock().unwrap() = key_set(keys);
        }

        fn fetches(&self) -> usize {
            self.fetches.load(Ordering::SeqCst)
        }
    }

    impl JwksSource for Arc<Heimdall> {
        fn fetch(&self) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            let body = self.published.lock().unwrap().clone();
            Box::pin(async move { Ok(body) })
        }
    }

    fn key_set(keys: &[&str]) -> String {
        format!(r#"{{"keys":[{}]}}"#, keys.join(","))
    }

    /// A clock the test moves by hand.
    #[derive(Clone)]
    struct Clock {
        start: Instant,
        elapsed: Arc<StdMutex<Duration>>,
    }

    impl Clock {
        fn new() -> Self {
            Self {
                start: Instant::now(),
                elapsed: Arc::new(StdMutex::new(Duration::ZERO)),
            }
        }

        fn advance(&self, by: Duration) {
            *self.elapsed.lock().unwrap() += by;
        }
    }

    /// Settings once the overlap is over: the key set and no secret.
    fn key_set_settings() -> AuthSettings {
        AuthSettings {
            mode: AuthMode::External,
            heimdall_jwks_url: "http://heimdall-api.test:8080/.well-known/jwks.json".to_string(),
            heimdall_scope_id: SCOPE.to_string(),
            ..AuthSettings::default()
        }
    }

    /// Settings during the overlap: the key set and the HS256 secret.
    fn overlap_settings() -> AuthSettings {
        AuthSettings {
            heimdall_token_secret: Secret::new(SECRET),
            ..key_set_settings()
        }
    }

    fn service_over(
        settings: &AuthSettings,
        heimdall: &Arc<Heimdall>,
        clock: &Clock,
    ) -> HeimdallAuthService {
        let clock = clock.clone();
        let key_set = JwksCache::new(
            Box::new(heimdall.clone()),
            Arc::new(move || clock.start + *clock.elapsed.lock().unwrap()),
        );
        HeimdallAuthService::with_key_set(settings, Some(key_set))
    }

    /// A valid `User` token signed ES256 with the key in `pem`, naming `kid`.
    fn es256_token(pem: &str, kid: Option<&str>, claims: serde_json::Value) -> String {
        let mut header = Header::new(Algorithm::ES256);
        header.kid = kid.map(str::to_string);
        encode(
            &header,
            &body_with(claims),
            &EncodingKey::from_ec_pem(pem.as_bytes()).expect("test key"),
        )
        .expect("encode")
    }

    async fn refused(service: &HeimdallAuthService, token: &str) -> bool {
        matches!(
            service.authenticate(token).await,
            Err(DomainError::Unauthorized)
        )
    }

    #[tokio::test]
    async fn given_es256_token_signed_with_a_published_key_when_authenticated_then_principal_is_the_id_claim(
    ) {
        let heimdall = Heimdall::publishing(&[KEY_A_JWK]);
        let service = service_over(&key_set_settings(), &heimdall, &Clock::new());

        let principal = service
            .authenticate(&es256_token(KEY_A_PEM, Some("key-a"), json!({})))
            .await
            .expect("accepted");

        assert_eq!(principal.user_id, PERSON);
        assert_eq!(heimdall.fetches(), 1);
    }

    #[tokio::test]
    async fn given_es256_token_signed_with_another_key_under_a_published_kid_when_authenticated_then_unauthorized(
    ) {
        let heimdall = Heimdall::publishing(&[KEY_A_JWK]);
        let service = service_over(&key_set_settings(), &heimdall, &Clock::new());

        assert!(refused(&service, &es256_token(KEY_B_PEM, Some("key-a"), json!({}))).await);
    }

    /// Every other check still applies to an ES256 token: here, the scope.
    #[tokio::test]
    async fn given_es256_token_from_another_scope_when_authenticated_then_unauthorized() {
        let heimdall = Heimdall::publishing(&[KEY_A_JWK]);
        let service = service_over(&key_set_settings(), &heimdall, &Clock::new());

        let token = es256_token(KEY_A_PEM, Some("key-a"), json!({ "scopeId": OTHER_SCOPE }));

        assert!(refused(&service, &token).await);
    }

    #[tokio::test]
    async fn given_es256_token_without_a_kid_when_authenticated_then_unauthorized_without_a_fetch()
    {
        let heimdall = Heimdall::publishing(&[KEY_A_JWK]);
        let service = service_over(&key_set_settings(), &heimdall, &Clock::new());

        assert!(refused(&service, &es256_token(KEY_A_PEM, None, json!({}))).await);
        assert_eq!(heimdall.fetches(), 0);
    }

    /// The algorithm-confusion attack against an asymmetric key: an HS256
    /// token whose HMAC secret is Heimdall's *public* key, which anyone can
    /// read from the key set. Its header names HS256, and the HS256 path
    /// verifies only against configured secrets — never a published key.
    #[tokio::test]
    async fn given_hs256_token_keyed_with_the_published_public_key_when_authenticated_then_unauthorized(
    ) {
        let heimdall = Heimdall::publishing(&[KEY_A_JWK]);
        let public_pem = "-----BEGIN PUBLIC KEY-----\nMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAExNYqHxZs5guFNq5tp/QAZDmyzd+r\nfqKOFuiH/X5XUOcqPfjFgBITTqwWAt8HWVRu8Yp4ztn6nuS0Dzb3rm9h1Q==\n-----END PUBLIC KEY-----\n";

        for settings in [key_set_settings(), overlap_settings()] {
            let service = service_over(&settings, &heimdall, &Clock::new());
            for forged_secret in [public_pem, KEY_A_JWK] {
                let mut header = Header::new(Algorithm::HS256);
                header.kid = Some("key-a".to_string());
                let token = encode(
                    &header,
                    &body_with(json!({})),
                    &EncodingKey::from_secret(forged_secret.as_bytes()),
                )
                .unwrap();

                assert!(refused(&service, &token).await);
            }
        }
    }

    /// An ES256 key under a header naming another asymmetric algorithm is
    /// refused rather than tried: only ES256 has a path.
    #[tokio::test]
    async fn given_a_token_naming_another_asymmetric_algorithm_when_authenticated_then_unauthorized(
    ) {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;

        let heimdall = Heimdall::publishing(&[KEY_A_JWK]);
        let service = service_over(&key_set_settings(), &heimdall, &Clock::new());
        let genuine = es256_token(KEY_A_PEM, Some("key-a"), json!({}));
        let (_, rest) = genuine.split_once('.').unwrap();

        for alg in ["ES384", "RS256", "PS256", "EdDSA"] {
            let header =
                URL_SAFE_NO_PAD.encode(format!(r#"{{"alg":"{alg}","kid":"key-a","typ":"JWT"}}"#));
            assert!(
                refused(&service, &format!("{header}.{rest}")).await,
                "{alg}"
            );
        }
        assert_eq!(
            heimdall.fetches(),
            0,
            "no other algorithm reaches the key set"
        );
    }

    /// A rotation: Heimdall starts signing with a key the cache has never
    /// seen. The unknown `kid` is what makes the cache fetch again.
    #[tokio::test]
    async fn given_a_token_naming_an_unknown_kid_when_authenticated_then_the_key_set_is_refetched()
    {
        let heimdall = Heimdall::publishing(&[KEY_A_JWK]);
        let clock = Clock::new();
        let service = service_over(&key_set_settings(), &heimdall, &clock);
        service
            .authenticate(&es256_token(KEY_A_PEM, Some("key-a"), json!({})))
            .await
            .expect("key A verifies");

        heimdall.publish(&[KEY_B_JWK, KEY_A_JWK]);
        clock.advance(Duration::from_secs(61));
        let principal = service
            .authenticate(&es256_token(KEY_B_PEM, Some("key-b"), json!({})))
            .await
            .expect("key B verifies once it is fetched");

        assert_eq!(principal.user_id, PERSON);
        assert_eq!(heimdall.fetches(), 2);
    }

    /// Made-up `kid`s cannot turn into a stream of requests to Heimdall: an
    /// unknown key is fetched for at most once a minute.
    #[tokio::test]
    async fn given_unknown_kids_within_a_minute_when_authenticated_then_the_key_set_is_fetched_once(
    ) {
        let heimdall = Heimdall::publishing(&[KEY_A_JWK]);
        let clock = Clock::new();
        let service = service_over(&key_set_settings(), &heimdall, &clock);

        for kid in ["made-up-1", "made-up-2", "made-up-3"] {
            assert!(refused(&service, &es256_token(KEY_A_PEM, Some(kid), json!({}))).await);
            clock.advance(Duration::from_secs(10));
        }
        assert_eq!(heimdall.fetches(), 1);

        clock.advance(Duration::from_secs(60));
        assert!(
            refused(
                &service,
                &es256_token(KEY_A_PEM, Some("made-up-4"), json!({}))
            )
            .await
        );
        assert_eq!(
            heimdall.fetches(),
            2,
            "a minute on, an unknown kid fetches again"
        );
    }

    /// A key Heimdall has withdrawn stops verifying once the cached set
    /// expires and is fetched again.
    #[tokio::test]
    async fn given_a_key_withdrawn_from_the_set_when_the_cache_expires_then_its_tokens_are_refused()
    {
        let heimdall = Heimdall::publishing(&[KEY_B_JWK, KEY_A_JWK]);
        let clock = Clock::new();
        let service = service_over(&key_set_settings(), &heimdall, &clock);
        let token_a = es256_token(KEY_A_PEM, Some("key-a"), json!({}));
        service
            .authenticate(&token_a)
            .await
            .expect("key A verifies");

        heimdall.publish(&[KEY_B_JWK]);
        clock.advance(crate::auth::jwks::MAX_AGE);

        assert!(refused(&service, &token_a).await);
        assert_eq!(heimdall.fetches(), 2);
    }

    /// The overlap: while the secret is configured, a token Heimdall signed
    /// HS256 before it moved is accepted alongside the ES256 ones.
    #[tokio::test]
    async fn given_the_overlap_when_hs256_and_es256_tokens_arrive_then_both_are_accepted() {
        let heimdall = Heimdall::publishing(&[KEY_A_JWK]);
        let service = service_over(&overlap_settings(), &heimdall, &Clock::new());

        assert!(service.authenticate(&valid_token()).await.is_ok());
        assert!(service
            .authenticate(&es256_token(KEY_A_PEM, Some("key-a"), json!({})))
            .await
            .is_ok());
    }

    /// After the overlap the secret is removed from the configuration, and a
    /// token signed with it is refused — the change needs no code.
    #[tokio::test]
    async fn given_the_overlap_is_over_when_an_hs256_token_arrives_then_unauthorized() {
        let heimdall = Heimdall::publishing(&[KEY_A_JWK]);
        let service = service_over(&key_set_settings(), &heimdall, &Clock::new());

        assert!(refused(&service, &valid_token()).await);
        assert_eq!(
            heimdall.fetches(),
            0,
            "an HS256 token never reaches the key set"
        );
    }

    /// With no key set configured — a Heimdall still on HS256 — an ES256 token
    /// has nothing to verify against.
    #[tokio::test]
    async fn given_no_key_set_configured_when_an_es256_token_arrives_then_unauthorized() {
        let service = HeimdallAuthService::from_settings(&settings());

        assert!(refused(&service, &es256_token(KEY_A_PEM, Some("key-a"), json!({}))).await);
    }
}
