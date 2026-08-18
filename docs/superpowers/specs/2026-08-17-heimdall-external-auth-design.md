# Design: Heimdall external authentication (F-09 — UC-36)

**Date:** 2026-08-17
**Status:** Approved, ready for implementation planning
**Tracks:** Replaces the placeholder implementation of an existing capability
area — Pluggable Authentication (AU). Milestone F-09.

## Context

External mode (UC-36 / FR-AU-02) exists but has never been pointed at a real
authentication service. `crates/alexandria-core/src/auth/external.rs` validates
a bearer JWT by reading the `kid` from its header, fetching a JWKS from a
configured `auth.jwks_url`, and checking the signature against the matching
key. Its own doc comment says as much: the structure UC-36 calls for, waiting
for a provider to be selected.

The provider is now selected. It is **Heimdall** (`ArturRios.Heimdall`), the
identity API this project already runs alongside, in which Alexandria will be
registered as an `Application` inside a `Scope`.

Three facts about Heimdall decide this design, all read from its source rather
than inferred from its OpenAPI document:

1. **It signs with HMAC-SHA256 and publishes no keys.** `JwtAuthTokenIssuer`
   signs with `SecurityAlgorithms.HmacSha256Signature` using a secret read from
   the `HEIMDALL_AUTH_TOKEN_SECRET` environment variable. There is no JWKS
   endpoint, no `/.well-known/` route, and no token-introspection endpoint
   anywhere in its API. It also writes **no `kid`**, deliberately — `Startup`
   leaves `SigningKeyId` unset so that a token names no key and validation
   tries each configured secret in turn.
2. **A registered `Application` carries no credential.** The `Application`
   entity holds a name, a scope, and an owner. Registering Alexandria in
   Heimdall is bookkeeping that records the relationship; it does not issue
   Alexandria a client id or secret it could authenticate with.
3. **Its tokens carry no `sub`.** The subject is an `id` claim, written by
   `IdentityUserMapper` alongside `role`, and — depending on the person's role —
   `scopeId`, `ownedScopeIds`, `scopePermissions`, and `mfaPending`.

Every one of those contradicts the current implementation. JWKS-and-`kid`
validation cannot verify a Heimdall token under any configuration, and even
given a correct key the service would reject every token for want of a `sub`
claim. This design replaces the mechanism.

Nothing about local mode changes. Alexandria remains single-owner, and exactly
one auth mode is active at runtime (FR-AU-01, FR-AU-03).

## Decisions

1. **The client authenticates with Heimdall directly; Alexandria only
   validates.** The Flutter front-end (or any other client) calls Heimdall's
   `POST /api/auth/login`, completes two-factor there if Heimdall demands it,
   and presents the resulting JWT to Alexandria as `Authorization: Bearer`.
   Alexandria never sees a password, never proxies a login, and gains no new
   endpoints. This is what the current external mode already assumes, and it
   keeps the credential-handling surface in the system whose job that is.

2. **The signature is verified offline against a shared HMAC secret.**
   Alexandria reads the same secret Heimdall signs with and verifies HS256
   locally. The alternatives were to migrate Heimdall to asymmetric signing and
   publish a JWKS, or to add an introspection endpoint to it — both correct for
   an identity provider serving several independent applications, and both
   substantial changes in another repository. For a single-user library server
   running beside its identity provider on one machine, sharing the secret is
   the proportionate answer.

   The trade is real and worth stating plainly: a process holding this secret
   can mint a token that **any** Heimdall-backed application will accept, not
   only Alexandria. This is acceptable here because both processes run under
   the same owner on the same host. It would not be acceptable if Heimdall ever
   served applications this owner does not control, and at that point the
   correct move is asymmetric signing with a published JWKS — which is why the
   verification step stays behind a single narrow function rather than being
   scattered through the service.

   Two benefits fall out of verifying locally. There is no network call on any
   request path, so authentication cannot be slowed or broken by Heimdall being
   busy or down; and the whole test suite runs offline against tokens minted
   in-process.

3. **Rotation is supported by accepting two secrets.** Heimdall's own
   `BuildJwtConfiguration` accepts a current and a previous secret so that
   replacing a signing key does not sign everybody out. Alexandria mirrors that
   exactly: it tries the current secret, then the previous one when configured.
   Without this, every secret rotation on the Heimdall side would black out
   Alexandria until its configuration was changed and the process restarted, in
   whichever order — a window this avoids entirely.

4. **A token is accepted on scope membership.** Alexandria's configuration
   names the UUID of the Heimdall scope it is registered in. A token is
   accepted when that UUID equals its `scopeId` claim (how a `User` carries the
   scope they belong to) or appears in its comma-separated `ownedScopeIds`
   claim (how a `ScopeAdmin` carries the scopes they own).

   Membership rather than a pinned person id, because access can then be
   granted and revoked in Heimdall by moving a person in or out of the scope,
   without editing Alexandria's configuration and restarting it. Membership
   rather than a named permission claim, because a permission only reaches a
   token when its `IncludeAsJwtClaim` flag is set — a second Heimdall-side
   invariant to maintain for no gain over the scope check at this scale.

   A **Heimdall `SystemAdmin` is refused**, and this is deliberate rather than
   incidental: they belong to no scope and own none, so they carry neither
   claim. Administering the identity provider is not the same as owning this
   library, and FR-AU-07 authorizes one owner over every catalog operation.

5. **A two-factor challenge token is refused.** When 2FA is pending, Heimdall's
   login returns `requiresTwoFactor` with a `challengeToken` **instead of** an
   authentication token, redeemable only at `POST /api/auth/2fa/verify`. That
   challenge token is a signed JWT carrying `id` and `role`, marked by an
   `mfaPending` claim set to the literal `"true"` — Heimdall itself relies on a
   global MVC filter to keep it away from its own endpoints. Alexandria makes
   the same check explicitly, or a caller who has proved one factor of two
   would be handed the entire catalog.

   Today `JwtTwoFactorChallengeTokenIssuer` builds its claims without a scope,
   so decision 4's check would refuse a challenge token even without this one.
   That is a property of Heimdall's current implementation rather than a
   guarantee of its API, and it is not what this check rests on.

6. **The signing algorithm is pinned to HS256.** The algorithm is taken from
   the service's configuration, never from the token's own header. Trusting the
   header is the classic JWT algorithm-confusion attack: `alg: none` asks to
   skip verification altogether, and an asymmetric `alg` invites a public key
   to be used as an HMAC secret. Since Heimdall writes no `kid` there is no key
   selection to attack, but the algorithm still has to be pinned.

7. **Every failure is an indistinguishable `401`.** A bad signature, an expired
   token, a wrong scope, a challenge token, and a malformed token all return
   `DomainError::Unauthorized` — a bare `401 unauthorized` with no reason code
   and no detail. FR-AU-12's machine-readable codes exist so a caller can act
   on input they can correct, such as a password that fails the strength
   policy; naming which of these checks failed tells an attacker which knob to
   turn next. Heimdall answers its own login failures (AF-11a…AF-11e)
   identically for the same reason.

   Misconfiguration is the opposite case and is treated as such: an absent
   secret or an unparseable scope id **fails startup** with a message naming
   the key, rather than leaving a process that answers `401` to every request
   forever with no indication why. Heimdall makes the same choice about the
   same secret.

## Architecture

The `AuthService` trait boundary does not move, so the change is contained.
`RuntimeAuthService::External` keeps its place in the enum and every handler,
the HTTP middleware, and the FFI surface are untouched; only the variant's
inner type changes.

### Components

**`HeimdallAuthService` (`crates/alexandria-core/src/auth/external.rs`)** —
replaces `ExternalAuthService<J>`. Holds the verification parameters resolved
once at startup: the secrets, the expected scope id, and the optional issuer
and audience. It has no type parameter and no collaborators, because it makes
no call to anything — the whole decision is a function of the token and this
configuration. Constructed via a fallible constructor that performs the
startup-time validation of decision 7.

**Deleted:** the `JwksProvider` trait, `HttpJwksProvider`, and the `Claims`
struct reading `sub`. There is no remaining consumer, and keeping a JWKS path
"in case" would mean carrying an untested second mechanism for a provider that
publishes no keys.

**`HeimdallClaims`** — a private deserialization struct for the claims
Alexandria reads: `id`, `scope_id`, `owned_scope_ids`, `mfa_pending`. All are
optional except in the checks that require them, because Heimdall omits a claim
rather than emitting it empty when it does not apply. `role` and
`scopePermissions` are not read; nothing in this design authorizes on them.

**`AuthSettings` (`crates/alexandria-core/src/config.rs`)** — `jwks_url` is
removed and the external-mode keys below are added.

**`services.rs`** — the `AuthMode::External` arm constructs a
`HeimdallAuthService` instead of an `ExternalAuthService<HttpJwksProvider>`. As
the constructor is fallible, the arm propagates its error; the surrounding
startup path already returns `Result`.

### Configuration

New `[auth]` keys, read only when `mode = "external"`:

| Key | Required | Purpose |
| --- | --- | --- |
| `heimdall_token_secret` | yes | The HS256 signing secret, matching Heimdall's `HEIMDALL_AUTH_TOKEN_SECRET`. |
| `heimdall_token_secret_previous` | no | Also accepted, mirroring Heimdall's `HEIMDALL_AUTH_TOKEN_SECRET_PREVIOUS`. Ignored when equal to the current secret, exactly as Heimdall ignores it. |
| `heimdall_scope_id` | yes | UUID of the Heimdall scope Alexandria is registered in. |
| `heimdall_issuer` | no | Validated only when set. |
| `heimdall_audience` | no | Validated only when set. |

Issuer and audience are optional because Heimdall reads both from environment
variables that default to empty, and signs tokens carrying neither when they
are unset. Requiring them here would reject every token from a default Heimdall
install; validating them when configured lets an operator who has set them on
the Heimdall side get the benefit.

Both secrets take the usual environment override
(`ALEXANDRIA_AUTH_HEIMDALL_TOKEN_SECRET`,
`ALEXANDRIA_AUTH_HEIMDALL_TOKEN_SECRET_PREVIOUS`), so a deployment need never
write a signing key to disk. `AuthSettings` derives `Debug`; the two secret
fields are held in a newtype whose `Debug` prints a redaction marker, so no
configuration dump or tracing span can emit a signing key. This is the same
instinct as FR-AU-06's ban on logging passwords, applied to the other secret
that grants the whole catalog.

### Data flow

Nothing is persisted and nothing is cached. On each request:

1. The HTTP middleware (`require_auth`) extracts the bearer token and calls
   `AuthService::authenticate`; the FFI surface reaches the same method through
   the same `RuntimeAuthService`.
2. `HeimdallAuthService::authenticate` runs the decision procedure below.
3. On success the handler receives a `Principal` whose `user_id` is the token's
   `id` claim — the person's Heimdall `PublicId`. On failure the caller gets
   `401`.

### The decision procedure

In order. Every failure returns `DomainError::Unauthorized`.

1. **Empty token** — a blank or whitespace-only bearer value is refused before
   anything is parsed.
2. **Header** — decode the header and require `alg = HS256`. Anything else,
   `none` included, is refused before a key is touched (decision 6).
3. **Signature and time claims** — verify against the current secret; if that
   fails and a previous secret is configured, verify against it. Validate `exp`
   and `nbf` with the library's default leeway. Validate `iss` and `aud` only
   when configured.
4. **Challenge token** — refuse when the `mfaPending` claim is present and
   `"true"` (decision 5).
5. **Scope** — require the configured scope id to equal the `scopeId` claim or
   to appear among the comma-separated `ownedScopeIds` (decision 4). A token
   carrying neither claim is refused.
6. **Subject** — require a non-empty `id` claim; it becomes
   `Principal.user_id`.

Steps 4 and 5 run after the signature check, never before: no claim is read for
a decision until the token has been shown to be authentic.

### Error handling

| Condition | Outcome |
| --- | --- |
| Any validation failure in steps 1–6 | `DomainError::Unauthorized` → `401`, body `{"error": "unauthorized"}`, no code |
| `mode = "local"` and a Heimdall JWT is presented | Already handled: local mode's service does not consult these rules at all (FR-AU-03, UC-36 AF-01) |
| `heimdall_token_secret` empty in external mode | Startup fails, naming the key |
| `heimdall_scope_id` empty or not a UUID in external mode | Startup fails, naming the key |

UC-36's AF-03 — "the external auth service is unreachable → service-unavailable"
— becomes unreachable and is removed. There is no call to be unavailable. This
is a genuine improvement in availability rather than a gap: Alexandria now
authenticates whether or not Heimdall is running, which for a local library
server is the behaviour an owner wants.

## Testing

Per the [Testing Specification Document](../../requirements/Testing%20Specification%20Document.md)
§6. The suite is entirely offline: tokens are minted in-process with
`jsonwebtoken`'s HS256 encoder, so no fake and no network are needed. This is
the direct payoff of local verification — the JWKS design would have required a
`JwksProvider` double for every one of these cases.

Unit tests over `HeimdallAuthService::authenticate`:

| Case | Expected |
| --- | --- |
| Valid token, `scopeId` matches | accepted; `Principal.user_id` is the `id` claim |
| Valid token, configured scope among `ownedScopeIds` | accepted |
| Valid token, scope is one of several in `ownedScopeIds` | accepted |
| Signed with the previous secret, which is configured | accepted |
| Signed with the previous secret, which is **not** configured | rejected |
| Signed with an unrelated secret | rejected |
| Expired (`exp` in the past) | rejected |
| Not yet valid (`nbf` in the future) | rejected |
| `alg` is not HS256 (`none`, and an asymmetric alg) | rejected |
| `mfaPending: "true"` and otherwise valid | rejected |
| `scopeId` is a different scope | rejected |
| Neither `scopeId` nor `ownedScopeIds` present (a `SystemAdmin`) | rejected |
| `id` claim missing or empty | rejected |
| Empty and whitespace-only bearer values | rejected |
| Structurally malformed token | rejected |
| `iss`/`aud` configured and matching | accepted |
| `iss`/`aud` configured and not matching | rejected |
| `iss`/`aud` not configured, token carries neither | accepted |
| `mode()` | returns `AuthMode::External` |

Configuration tests: external mode with no secret fails startup; external mode
with an absent or non-UUID scope id fails startup; a previous secret equal to
the current one is ignored; the `Debug` output of `AuthSettings` contains
neither secret.

No integration test is added. There is nothing to integrate with — the service
makes no call — and the existing HTTP integration suite already covers the
middleware's `401` path through the `AuthService` trait.

A **manual smoke check** is documented for the point at which Alexandria is
actually registered in Heimdall, since only a live token proves the claim names
match: register the application in a scope, log in to Heimdall as a person in
that scope, set Alexandria's secret and scope id to match, and call an
authenticated Alexandria endpoint with the returned token.

## Documentation updates

- **System Requirements Document** — FR-AU-02 is reworded from validating "against
  the external authentication service" to verifying the external provider's
  signed token against a configured signing secret and accepting it on scope
  membership. §4 (configuration) gains the new keys and loses `jwks_url`.
- **Use Case Specification Document** — UC-36's main flow step 3 gains the
  algorithm, challenge-token, and scope checks; AF-03 (service unreachable) is
  removed as unreachable; a new alternative flow covers a token that is valid
  but outside the configured scope.
- **Operations & Infrastructure Document** §4 — the configuration table.
- `config.toml.example` — the new keys, with the secret-handling note.
- **README** — the F-09 row, and UC-36's entry.

## Out of scope

Named because they were discussed, and each gets its own spec:

- **Recovery codes for local mode**, replacing the e-mail confirmation and
  password-reset flows merged in `b9164ad` (FR-AU-13…FR-AU-19).
- **Windows credential login**, including how a third authentication path
  coexists with FR-AU-01's single-active-mode rule.

Also out of scope here: any change to Heimdall itself, proxying Heimdall's
login through Alexandria, and authorizing on Heimdall roles or scope
permissions.
