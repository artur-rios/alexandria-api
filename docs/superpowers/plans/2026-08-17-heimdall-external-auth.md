# Heimdall External Authentication Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace external mode's placeholder JWKS validation with offline HS256 verification of Heimdall-issued JWTs, accepted on membership of a configured Heimdall scope.

**Architecture:** `HeimdallAuthService` replaces `ExternalAuthService<J>` behind the unchanged `AuthService` trait, so no handler, no middleware, and no FFI export moves. Verification is a pure function of the token and startup configuration — no network call on any request path — which also means the whole test suite mints its own tokens in-process and runs offline.

**Tech Stack:** Rust 2021, `jsonwebtoken` 9 (HS256 decode + test-side encode), `uuid` 1, `serde`/`serde_json`, `base64` 0.22 (dev-only, for the `alg: none` test vector).

## Global Constraints

- **Design spec:** [`docs/superpowers/specs/2026-08-17-heimdall-external-auth-design.md`](../specs/2026-08-17-heimdall-external-auth-design.md). Every decision below traces to it; do not improvise past it.
- **Every authentication failure is `DomainError::Unauthorized`.** No reason codes, no distinguishing detail, no logging of which check failed. A bad signature, an expired token, a wrong scope, and a challenge token are indistinguishable to the caller.
- **Never log a signing secret or a token.** Secrets live behind the `Secret` newtype whose `Debug` redacts.
- **The algorithm is HS256, taken from configuration, never from the token header.**
- **Test naming:** `given_<condition>_when_<action>_then_<outcome>`, per the [Testing Specification Document](../../requirements/Testing%20Specification%20Document.md) §5.
- **No new mocking library.** Hand-written fakes or `mockall` only. This feature needs neither — it has no collaborators.
- **Run from the repo root.** `cargo test -p alexandria-core` builds the FFmpeg-linked core crate; it is slow on a cold cache, which is expected.
- **Commit messages:** lowercase Conventional Commits subject, ≤50 chars, imperative, body wrapped at 72.

---

### Task 1: Configuration keys for Heimdall verification

Adds the new `[auth]` keys and the startup validation, leaving `jwks_url` in
place so the existing `services.rs` keeps compiling. Task 3 removes it.

**Files:**
- Modify: `crates/alexandria-core/src/config.rs`
- Modify: `crates/alexandria-core/tests/config.rs`
- Modify: `config.toml.example`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `pub struct Secret(String)` with `Secret::new(impl Into<String>) -> Secret`, `Secret::expose(&self) -> &str`, `Secret::is_empty(&self) -> bool`, and a redacting `Debug`.
  - `AuthSettings` fields `heimdall_token_secret: Secret`, `heimdall_token_secret_previous: Secret`, `heimdall_scope_id: String`, `heimdall_issuer: String`, `heimdall_audience: String`.
  - `AuthSettings::validate(&self) -> Result<(), DomainError>`.

- [ ] **Step 1: Write the failing tests**

First widen the file's existing import from
`use alexandria_core::config::{AuthMode, Settings};` to
`use alexandria_core::config::{AuthMode, AuthSettings, Secret, Settings};`,
then append:

```rust
/// The external-mode keys parse from the `[auth]` section, including the two
/// secrets, so an operator can configure verification entirely from the file.
#[test]
fn given_heimdall_keys_when_parsed_then_external_settings_match() {
    let toml = r#"
[auth]
mode = "external"
heimdall_token_secret = "current-secret"
heimdall_token_secret_previous = "previous-secret"
heimdall_scope_id = "0b8d3a6e-4a1f-4c2b-9f1e-7c5d2a9b3e40"
heimdall_issuer = "heimdall"
heimdall_audience = "alexandria"
"#;
    let settings: Settings = toml::from_str(toml).unwrap();

    assert_eq!(settings.auth.mode, AuthMode::External);
    assert_eq!(settings.auth.heimdall_token_secret.expose(), "current-secret");
    assert_eq!(
        settings.auth.heimdall_token_secret_previous.expose(),
        "previous-secret"
    );
    assert_eq!(
        settings.auth.heimdall_scope_id,
        "0b8d3a6e-4a1f-4c2b-9f1e-7c5d2a9b3e40"
    );
    assert_eq!(settings.auth.heimdall_issuer, "heimdall");
    assert_eq!(settings.auth.heimdall_audience, "alexandria");
}

/// Omitted external keys are empty rather than an error: a local-mode install
/// never sets them, and `validate` is what refuses an external-mode process
/// that has left them out.
#[test]
fn given_no_heimdall_keys_when_parsed_then_empty() {
    let settings: Settings = toml::from_str("[auth]\nmode = \"local\"\n").unwrap();

    assert!(settings.auth.heimdall_token_secret.is_empty());
    assert!(settings.auth.heimdall_token_secret_previous.is_empty());
    assert_eq!(settings.auth.heimdall_scope_id, "");
}

/// A signing secret must never reach a log. `AuthSettings` derives `Debug`,
/// and a tracing span or a config dump would otherwise emit the one value
/// that grants the whole catalog — the same reasoning as FR-AU-06's ban on
/// logging passwords.
#[test]
fn given_configured_secrets_when_debug_formatted_then_redacted() {
    let mut auth = AuthSettings::default();
    auth.heimdall_token_secret = Secret::new("super-secret-value");
    auth.heimdall_token_secret_previous = Secret::new("older-secret-value");

    let rendered = format!("{auth:?}");

    assert!(!rendered.contains("super-secret-value"));
    assert!(!rendered.contains("older-secret-value"));
    assert!(rendered.contains("redacted"));
}

/// Local mode never reads the Heimdall keys, so it validates whatever it has.
#[test]
fn given_local_mode_when_validated_then_ok_without_heimdall_keys() {
    let auth = AuthSettings {
        mode: AuthMode::Local,
        ..AuthSettings::default()
    };

    assert!(auth.validate().is_ok());
}

/// A process that cannot verify a token must refuse to start, rather than
/// answer 401 to every request forever with no indication why.
#[test]
fn given_external_mode_without_secret_when_validated_then_error_names_the_key() {
    let auth = AuthSettings {
        mode: AuthMode::External,
        heimdall_scope_id: "0b8d3a6e-4a1f-4c2b-9f1e-7c5d2a9b3e40".to_string(),
        ..AuthSettings::default()
    };

    let message = auth.validate().unwrap_err().to_string();

    assert!(message.contains("auth.heimdall_token_secret"), "{message}");
}

/// External mode accepts a token on membership of a named scope, so the
/// configured value has to be a UUID for the comparison to mean anything.
#[test]
fn given_external_mode_with_non_uuid_scope_when_validated_then_error_names_the_key() {
    let auth = AuthSettings {
        mode: AuthMode::External,
        heimdall_token_secret: Secret::new("current-secret"),
        heimdall_scope_id: "not-a-uuid".to_string(),
        ..AuthSettings::default()
    };

    let message = auth.validate().unwrap_err().to_string();

    assert!(message.contains("auth.heimdall_scope_id"), "{message}");
}

#[test]
fn given_external_mode_fully_configured_when_validated_then_ok() {
    let auth = AuthSettings {
        mode: AuthMode::External,
        heimdall_token_secret: Secret::new("current-secret"),
        heimdall_scope_id: "0b8d3a6e-4a1f-4c2b-9f1e-7c5d2a9b3e40".to_string(),
        ..AuthSettings::default()
    };

    assert!(auth.validate().is_ok());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p alexandria-core --test config`
Expected: FAIL — `cannot find type Secret in ... config`, `no method named validate`.

- [ ] **Step 3: Add the `Secret` newtype**

In `crates/alexandria-core/src/config.rs`, add `use std::fmt;` and
`use uuid::Uuid;` to the imports at the top, then add above `AuthSettings`:

```rust
/// A configuration value that must never reach a log.
///
/// `Debug` prints a marker instead of the value, so a config dump or a
/// `tracing` span cannot emit a signing secret. This is FR-AU-06's ban on
/// logging passwords applied to the other secret that grants the whole
/// catalog — with the difference that this one is *shared* with Heimdall, so
/// leaking it compromises more than Alexandria.
#[derive(Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The plaintext. Named so that every read site is obvious at a glance.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the secret is unset. Whitespace counts as unset: a key
    /// configured to `" "` is a mistake, not a secret.
    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.0.is_empty() {
            "Secret(unset)"
        } else {
            "Secret(redacted)"
        })
    }
}
```

- [ ] **Step 4: Add the fields to `AuthSettings`**

Insert into the `AuthSettings` struct, after the `jwks_url` field:

```rust
    /// External mode only: the HS256 secret Heimdall signs its tokens with
    /// (its `HEIMDALL_AUTH_TOKEN_SECRET`). Required in external mode —
    /// Heimdall publishes no keys, so this is the only way to verify one of
    /// its tokens.
    #[serde(default)]
    pub heimdall_token_secret: Secret,
    /// External mode only: the secret Heimdall is rotating away from (its
    /// `HEIMDALL_AUTH_TOKEN_SECRET_PREVIOUS`). Accepted alongside the current
    /// one, mirroring Heimdall's own two-key scheme, so a rotation there does
    /// not black out Alexandria until this file is edited and the process
    /// restarted. Ignored when equal to the current secret: the same value
    /// under two names is not a rotation.
    #[serde(default)]
    pub heimdall_token_secret_previous: Secret,
    /// External mode only: the UUID of the Heimdall scope Alexandria is
    /// registered in. A token is accepted when it names this scope.
    #[serde(default)]
    pub heimdall_scope_id: String,
    /// External mode only: the `iss` claim to require, checked only when set.
    /// Heimdall reads its issuer from an environment variable that defaults
    /// to empty and then signs tokens carrying no `iss` at all, so requiring
    /// one unconditionally would reject every token from a default install.
    #[serde(default)]
    pub heimdall_issuer: String,
    /// External mode only: the `aud` claim to require, checked only when set,
    /// for the same reason as `heimdall_issuer`.
    #[serde(default)]
    pub heimdall_audience: String,
```

And in `impl Default for AuthSettings`, after `jwks_url: String::new(),`:

```rust
            heimdall_token_secret: Secret::default(),
            heimdall_token_secret_previous: Secret::default(),
            heimdall_scope_id: String::new(),
            heimdall_issuer: String::new(),
            heimdall_audience: String::new(),
```

- [ ] **Step 5: Add `AuthSettings::validate`**

Add after the `impl Default for AuthSettings` block:

```rust
impl AuthSettings {
    /// Startup validation for external mode (UC-36). Each binary calls this
    /// before building services: a process that cannot verify a token must
    /// refuse to start, rather than answer `401` to every request forever
    /// with nothing to say why. Heimdall makes the same choice about the same
    /// secret, and for the same reason.
    ///
    /// Local mode reads none of these keys and always passes.
    pub fn validate(&self) -> Result<(), DomainError> {
        if self.mode != AuthMode::External {
            return Ok(());
        }

        if self.heimdall_token_secret.is_empty() {
            return Err(DomainError::Config(
                "auth.heimdall_token_secret is unset: external mode verifies Heimdall's \
                 tokens against the secret it signs them with, and Heimdall publishes no \
                 keys to fetch instead"
                    .to_string(),
            ));
        }

        Uuid::parse_str(self.heimdall_scope_id.trim()).map_err(|_| {
            DomainError::Config(format!(
                "auth.heimdall_scope_id is not a UUID: {:?}. External mode accepts a token \
                 on membership of this Heimdall scope, so it must name one.",
                self.heimdall_scope_id
            ))
        })?;

        Ok(())
    }
}
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p alexandria-core --test config`
Expected: PASS, all tests in the file.

- [ ] **Step 7: Document the keys in the example config**

In `config.toml.example`, insert after the `jwks_url` block:

```toml
# External mode only: the HS256 secret Heimdall signs its tokens with — the
# same value as Heimdall's own HEIMDALL_AUTH_TOKEN_SECRET. Required in external
# mode: Heimdall publishes no key set, so this is the only way to verify one of
# its tokens. Prefer the ALEXANDRIA_AUTH_HEIMDALL_TOKEN_SECRET environment
# variable, so a signing key need never be written to disk. Ignored when
# mode = "local".
heimdall_token_secret = ""

# External mode only: the secret Heimdall is rotating away from (its
# HEIMDALL_AUTH_TOKEN_SECRET_PREVIOUS). Tokens signed with either secret are
# accepted, so a rotation on the Heimdall side does not black out Alexandria.
# Ignored when equal to heimdall_token_secret.
heimdall_token_secret_previous = ""

# External mode only: the UUID of the Heimdall scope Alexandria is registered
# in. A token is accepted when it names this scope — as the scope its holder
# belongs to, or as one of the scopes they own. Required in external mode.
heimdall_scope_id = ""

# External mode only: the issuer and audience to require, each checked only
# when set. Heimdall leaves both empty by default and then signs tokens
# carrying neither, so requiring them unconditionally would reject every token
# from a default install.
heimdall_issuer = ""
heimdall_audience = ""
```

- [ ] **Step 8: Commit**

```bash
git add crates/alexandria-core/src/config.rs crates/alexandria-core/tests/config.rs config.toml.example
git commit -m "feat: configure Heimdall token verification"
```

---

### Task 2: `HeimdallAuthService`

Builds the new service and its full test suite alongside the old one, which
`services.rs` still uses. Nothing is wired yet, so this task changes no
runtime behaviour and can be reviewed purely on the decision procedure.

**Files:**
- Create: `crates/alexandria-core/src/auth/heimdall.rs`
- Modify: `crates/alexandria-core/src/auth/mod.rs` (declare the module)
- Modify: `crates/alexandria-core/Cargo.toml` (dev-dependency `base64`)

**Interfaces:**
- Consumes: `Secret`, the five `AuthSettings` Heimdall fields (Task 1).
- Produces:
  - `pub struct HeimdallAuthService` (`Clone`), with `HeimdallAuthService::from_settings(&AuthSettings) -> HeimdallAuthService`.
  - It implements `AuthService`: `authenticate(&self, token: &str) -> Result<Principal, DomainError>` and `mode(&self) -> AuthMode`.

A new file rather than a rewrite in place: `external.rs` stays untouched and
compiling until Task 3 deletes it, so this task and its neighbour can each be
accepted or rejected on their own.

- [ ] **Step 1: Add the dev-dependency**

In `crates/alexandria-core/Cargo.toml`, under `[dev-dependencies]`, add:

```toml
# Tests only: `jsonwebtoken` will not encode an `alg: none` token — refusing
# to produce one is the point of the library — so the test that proves we
# reject it assembles the token by hand.
base64.workspace = true
```

- [ ] **Step 2: Declare the module**

In `crates/alexandria-core/src/auth/mod.rs`, add `pub mod heimdall;` to the
module list, keeping it alphabetical (after `pub mod external;`).

- [ ] **Step 3: Write the failing tests**

Create `crates/alexandria-core/src/auth/heimdall.rs` containing **only** the
test module for now:

```rust
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

    #[tokio::test]
    async fn given_service_when_mode_queried_then_external() {
        assert_eq!(
            HeimdallAuthService::from_settings(&settings()).mode(),
            AuthMode::External
        );
    }
}
```

- [ ] **Step 4: Run the tests to verify they fail**

Run: `cargo test -p alexandria-core heimdall`
Expected: FAIL to compile — `cannot find type HeimdallAuthService in this scope`.

- [ ] **Step 5: Write the implementation**

Prepend to `crates/alexandria-core/src/auth/heimdall.rs`, above the test
module:

```rust
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
    fn validation(&self) -> Validation {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.validate_exp = true;
        validation.validate_nbf = true;
        // `exp` is the only claim Heimdall always writes and the only one
        // whose absence must not be treated as "fine".
        validation.required_spec_claims = HashSet::from(["exp".to_string()]);

        // `Validation::new` starts with no issuer, so the unconfigured arm has
        // nothing to undo — unlike the audience, which defaults to being
        // validated.
        if let Some(issuer) = &self.issuer {
            validation.set_issuer(&[issuer]);
        }
        match &self.audience {
            Some(audience) => validation.set_audience(&[audience]),
            None => validation.validate_aud = false,
        }

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
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p alexandria-core heimdall`
Expected: PASS, 19 tests.

- [ ] **Step 7: Check lints**

Run: `cargo clippy -p alexandria-core --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add crates/alexandria-core/src/auth/heimdall.rs crates/alexandria-core/src/auth/mod.rs crates/alexandria-core/Cargo.toml
git commit -m "feat: verify Heimdall tokens offline"
```

---

### Task 3: Wire it up and delete the JWKS path

Switches external mode over and removes everything the old mechanism needed —
including `reqwest`, whose only use in the whole workspace was fetching a JWKS.

**Files:**
- Delete: `crates/alexandria-core/src/auth/external.rs`
- Modify: `crates/alexandria-core/src/auth/mod.rs`
- Modify: `crates/alexandria-core/src/services.rs:375-377`
- Modify: `crates/alexandria-core/src/config.rs` (drop `jwks_url`)
- Modify: `crates/alexandria-core/tests/config.rs` (drop `jwks_url`)
- Modify: `crates/alexandria-core/Cargo.toml` (drop `reqwest`)
- Modify: `Cargo.toml` (stale comment on `futures-util`)
- Modify: `config.toml.example`

**Interfaces:**
- Consumes: `HeimdallAuthService::from_settings` (Task 2).
- Produces: `RuntimeAuthService::External(HeimdallAuthService)` — the variant keeps its name and its position, so no consumer changes.

- [ ] **Step 1: Point `RuntimeAuthService` at the new service**

In `crates/alexandria-core/src/auth/mod.rs`: delete the `pub mod external;`
line, and in the `RuntimeAuthService` enum replace

```rust
    External(external::ExternalAuthService<external::HttpJwksProvider>),
```

with

```rust
    External(heimdall::HeimdallAuthService),
```

- [ ] **Step 2: Rewire `build_services`**

In `crates/alexandria-core/src/services.rs`, replace the import

```rust
use crate::auth::external::{ExternalAuthService, HttpJwksProvider};
```

with

```rust
use crate::auth::heimdall::HeimdallAuthService;
```

and replace the `AuthMode::External` arm:

```rust
        // UC-36: Heimdall signs HS256 with a secret this process is
        // configured with, so verification is offline and needs no
        // collaborator. `AuthSettings::validate` has already refused to let a
        // misconfigured process get this far.
        AuthMode::External => {
            RuntimeAuthService::External(HeimdallAuthService::from_settings(&settings.auth))
        }
```

- [ ] **Step 3: Delete the JWKS implementation**

```bash
git rm crates/alexandria-core/src/auth/external.rs
```

- [ ] **Step 4: Drop `jwks_url`**

In `crates/alexandria-core/src/config.rs`, delete the `pub jwks_url: String,`
field and its `#[serde(default)]`, the `jwks_url: String::new(),` line in
`Default`, and the `ALEXANDRIA_AUTH_JWKS_URL` block in `apply_env_overrides`.

In `crates/alexandria-core/tests/config.rs`, delete the
`jwks_url = "https://example.invalid/jwks"` line from the TOML fixture and the
`assert_eq!(settings.auth.jwks_url, ...)` assertion.

In `config.toml.example`, delete the `jwks_url` key and its comment block.

- [ ] **Step 5: Drop the `reqwest` dependency**

`external.rs` was its only user anywhere in the workspace. Delete
`reqwest.workspace = true` from `crates/alexandria-core/Cargo.toml`.

In the root `Cargo.toml`, the `futures-util` comment claims it is "Already in
the dependency graph via sqlx and reqwest". Amend it to name `sqlx` alone:

```toml
# Bounded-concurrency stream combinators (`buffer_unordered`) for the indexer.
# Already in the dependency graph via sqlx, so this adds no new transitive
# weight — only a direct, declared use of it.
```

Leave `reqwest` in the `[workspace.dependencies]` table: it is the declaration
of a version, not a dependency, and the mail provider integration will want it.

- [ ] **Step 6: Verify the whole workspace builds and passes**

Run: `cargo test --workspace`
Expected: PASS. Nothing outside these files should need changing — the trait
boundary did not move.

- [ ] **Step 7: Check lints and formatting**

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "feat: authenticate external mode against Heimdall"
```

---

### Task 4: Refuse to start a misconfigured external-mode process

`AuthSettings::validate` exists but nothing calls it. Both binaries load
settings with `load_or_default`, which swallows errors, so this is a deliberate
explicit call rather than something to bury in `load`.

**Files:**
- Modify: `crates/alexandria-http/src/main.rs:19-29`
- Modify: `crates/alexandria-ffi/src/lib.rs:173-189`
- Test: `crates/alexandria-core/tests/config.rs` (already covers `validate` itself, from Task 1)

**Interfaces:**
- Consumes: `AuthSettings::validate(&self) -> Result<(), DomainError>` (Task 1).
- Produces: nothing later tasks use.

- [ ] **Step 1: Fail the HTTP binary's startup**

In `crates/alexandria-http/src/main.rs`, immediately after
`init_tracing(&settings.logging.level);` and before the bind address is read,
insert:

```rust
    // UC-36: external mode cannot verify a token without the Heimdall signing
    // secret and the scope it accepts. Refuse to start rather than answer 401
    // to every request for the life of the process.
    settings.auth.validate()?;
```

`main` already returns `anyhow::Result<()>`, and `DomainError` implements
`std::error::Error`, so `?` converts it and the process exits non-zero with
the message.

- [ ] **Step 2: Fail the FFI initializer**

In `crates/alexandria-ffi/src/lib.rs`, inside `alexandria_index_init`, after
`settings.database.path = path.clone();`, insert:

```rust
    // Same gate as the HTTP binary: a misconfigured external mode is a
    // startup failure on both surfaces (FR-AU-08).
    if settings.auth.validate().is_err() {
        return INDEX_ERR_OTHER;
    }
```

- [ ] **Step 3: Verify both surfaces still start**

Run: `cargo test --workspace`
Expected: PASS. The integration suites build settings with `mode: Local` or
defaults and never call `alexandria_index_init` in external mode, so none of
them trips the new gate.

- [ ] **Step 4: Confirm the gate actually fires**

Run:

```bash
ALEXANDRIA_AUTH_MODE=external ALEXANDRIA_CONFIG=/nonexistent.toml cargo run -p alexandria-http
```

Expected: exits non-zero, printing a message naming `auth.heimdall_token_secret`.
Confirm it does not bind a port.

- [ ] **Step 5: Commit**

```bash
git add crates/alexandria-http/src/main.rs crates/alexandria-ffi/src/lib.rs
git commit -m "feat: refuse an unverifiable external mode"
```

---

### Task 5: Documentation

**Files:**
- Modify: `docs/requirements/System Requirements Document.md` (FR-AU-02, FR-AU-03; §4 configuration table if present there)
- Modify: `docs/requirements/Use Case Specification Document.md` (UC-36)
- Modify: `docs/requirements/Operations & Infrastructure Document.md:145` (configuration table)
- Modify: `README.md` (F-09 table row for UC-36)

- [ ] **Step 1: Reword FR-AU-02**

In `docs/requirements/System Requirements Document.md`, replace FR-AU-02:

> | FR-AU-02 | In external mode, the system shall verify each caller's JWT against a configured signing secret shared with the external authentication service, and shall accept the caller as the owner only when the token names the configured scope. |

Leave FR-AU-03 as it stands: rejecting credentials presented via the inactive
mode is unchanged.

- [ ] **Step 2: Update UC-36**

In `docs/requirements/Use Case Specification Document.md`, replace UC-36's
main flow step 3 and its alternative-flow table:

```markdown
3. The system verifies the JWT's signature against the configured signing secret, requiring the configured algorithm rather than the one the token's header names, and validates its expiry and not-before times (and its issuer and audience when those are configured).
4. The system confirms the token is a full authentication token rather than a two-factor challenge, and that it names the configured scope — as the scope its holder belongs to, or as one of the scopes they own.
5. The system authenticates the caller as the owner and proceeds with the requested operation.
```

| ID | Condition | Outcome |
| --- | --- | --- |
| AF-01 | The active auth mode is local login (external JWT inactive) | The system rejects the JWT with an unauthorized error. |
| AF-02 | The JWT is missing, malformed, expired, not yet valid, signed with an unconfigured key, or names an algorithm other than the configured one | The system denies with an unauthorized error. |
| AF-03 | The JWT is a two-factor challenge token rather than a full authentication token | The system denies with an unauthorized error. |
| AF-04 | The JWT is valid but names no scope, or a scope other than the configured one | The system denies with an unauthorized error. |

Add below the table:

> Every alternative flow answers identically — an unauthorized error with no
> reason code — so a caller cannot learn which check refused them. External
> mode makes no call to the external service, so there is no unreachable-service
> outcome: verification is offline against a configured secret, and Alexandria
> authenticates whether or not that service is running. Configuration that
> makes verification impossible is a startup failure, not a per-request one.

Renumber the "Preconditions" row's mention of AF ids only if one exists; the
existing AF-03 (external auth service unreachable) is replaced by the rows
above.

- [ ] **Step 3: Update the operations configuration table**

In `docs/requirements/Operations & Infrastructure Document.md`, replace the
`auth.jwks_url` row (line ~145) with:

| Key | Source | Notes |
| --- | --- | --- |
| `auth.heimdall_token_secret` | config / `ALEXANDRIA_AUTH_HEIMDALL_TOKEN_SECRET` | external mode only; the HS256 secret Heimdall signs with. Required — startup fails without it. Prefer the environment variable; never commit it. |
| `auth.heimdall_token_secret_previous` | config / `ALEXANDRIA_AUTH_HEIMDALL_TOKEN_SECRET_PREVIOUS` | external mode only; the secret Heimdall is rotating away from. Both are accepted, so a rotation there causes no outage here. |
| `auth.heimdall_scope_id` | config / `ALEXANDRIA_AUTH_HEIMDALL_SCOPE_ID` | external mode only; UUID of the Heimdall scope whose members are accepted as the owner. Required — startup fails without it. |
| `auth.heimdall_issuer` | config / `ALEXANDRIA_AUTH_HEIMDALL_ISSUER` | external mode only; checked only when set. |
| `auth.heimdall_audience` | config / `ALEXANDRIA_AUTH_HEIMDALL_AUDIENCE` | external mode only; checked only when set. |

- [ ] **Step 4: Add the registration and smoke-check runbook**

Append to the same document's authentication section:

```markdown
#### Registering Alexandria with Heimdall (external mode)

1. In Heimdall, as a Scope Admin who owns the scope, `POST /api/scopes/{scopeId}/applications` with Alexandria's name. This records the relationship; it issues no credential, and Alexandria needs none.
2. Copy Heimdall's `HEIMDALL_AUTH_TOKEN_SECRET` into Alexandria's `ALEXANDRIA_AUTH_HEIMDALL_TOKEN_SECRET`, and the scope's UUID into `ALEXANDRIA_AUTH_HEIMDALL_SCOPE_ID`.
3. Smoke-check the pair: log in to Heimdall as a person in that scope (`POST /api/auth/login`, completing two-factor if enabled) and call any authenticated Alexandria endpoint with the returned token. A `200` confirms the claim names and the secret match; a `401` means one of them does not.
4. To rotate the secret: set `ALEXANDRIA_AUTH_HEIMDALL_TOKEN_SECRET_PREVIOUS` to the secret in use, set the current one to the new value on both sides, restart both, and clear the previous variable after one token lifetime.
```

- [ ] **Step 5: Update the README**

In `README.md`, in the F-09 table, change UC-36's title from "Authenticate via
external JWT" to "Authenticate via Heimdall JWT". Leave the checkbox checked
and the issue reference as it is.

- [ ] **Step 6: Verify the docs are consistent**

Run: `grep -rn "jwks" README.md docs/requirements config.toml.example`
Expected: no matches outside `docs/superpowers/` (the spec and this plan
describe the mechanism being replaced, and keep their references).

- [ ] **Step 7: Commit**

```bash
git add README.md docs/requirements config.toml.example
git commit -m "docs: describe Heimdall external authentication"
```

---

## Self-review notes

Checked against the spec:

- Decisions 1 (client authenticates directly) and 2 (offline HS256) — Tasks 1–3; no new endpoint is added anywhere, which is decision 1's whole content.
- Decision 3 (two-secret rotation) — Task 1 config, Task 2 `from_settings` plus three tests, Task 5 runbook.
- Decision 4 (scope membership, SystemAdmin refused) — Task 2 `names_configured_scope` plus four tests.
- Decision 5 (challenge token) — Task 2, one test.
- Decision 6 (algorithm pinning) — Task 2, two tests (`HS512`, `none`).
- Decision 7 (uniform 401; misconfiguration fails startup) — Task 1 `validate`, Task 4 both binaries, and every test asserts the same `Unauthorized`.
- Documentation updates listed in the spec — Task 5, all five files.

Deliberately not in this plan, and not in the spec: any change to Heimdall,
proxying its login, and authorizing on `role` or `scopePermissions`.
