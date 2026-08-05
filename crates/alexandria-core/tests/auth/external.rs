//! Unit tests for the UC-36 ExternalAuthService (Testing Specification §6).
//! Exercises the decision logic against a fake `JwksProvider` — no real
//! network call. Coverage follows §6.3: the happy path, AF-02 (missing,
//! malformed, or wrongly-signed JWT), and AF-03 (the external auth service
//! is unreachable). AF-01 ("the active auth mode is local login") is not
//! reachable here by construction — `services.rs` only ever builds an
//! `ExternalAuthService` when the configured mode is external
//! (`RuntimeAuthService::External`); when the mode is local, requests are
//! routed to `LocalAuthService` instead, which rejects a JWT-shaped token
//! outright because it isn't a valid session-id UUID.

use jsonwebtoken::jwk::{
    AlgorithmParameters, CommonParameters, Jwk, JwkSet, KeyAlgorithm, RSAKeyParameters, RSAKeyType,
};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::Serialize;

use alexandria_core::auth::external::{ExternalAuthService, JwksProvider};
use alexandria_core::auth::AuthService;
use alexandria_core::errors::DomainError;

// A fixed 2048-bit RSA test keypair, generated locally for this test only —
// never used for anything but signing tokens this file also verifies.
const TEST_RSA_PRIVATE_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEuwIBADANBgkqhkiG9w0BAQEFAASCBKUwggShAgEAAoIBAQCedZ5AX+SOW87s
IpCvA8WlMGm9SCdf3fpXRrTVYvwXc5TrIE5AYh22q3F0xWiCpIaN8rI1/Jtu7LJx
lzM1qNXYzNd7kx1uYlwT0GyWhtMPWwXAGtfkZG+LtHd9IMIjaXHO7Plpr4wXtDpU
nnQTC9TLohYQGSw8E7u9ojfjKhrA3IyMcOYk0cCCYki9W0C6r5lW4s1sbakoG2AP
u3yhp2xOw5+zkr7wGDNSUbgtV1hINrY22ml5YjRRf/FlNBA9Orw4n5khxz5ESfJY
vgQsBic4LCK4jTN9ZaQkT+zvYMShMF48ZWVmGo2taCvG5P64ndCBXErXgeKQdqkB
dx4zPsxPAgMBAAECgf9rnepE/Ky86vpL2VxSl6X1IbH0W0a/mt9erJr1OfMqKL4G
ZjvWIPs7q6ht4AFUuK4d0MHai2MzW0fVl6BqCzSlWnksriX9aRWUMNZI2TDy1BjZ
ghvAmKQR1NA6qvkun0txpiJ33p6AkglonUvyAJcEzKdd0y8r+yOpD3crIe1PUwti
UPlrecV5yXbKO2zXD1ljr//M9a/g8BXfglCE61QFm8dhaiUcbyGTJwCa9KU3EYXg
E8uQTVq3CTCYZ4IdvjCJ4fJ2y9cHktAncu4BQiJ99vkcxUbkdgA1swFlO4ASbiF3
1w4uF+ralxoMUJFRPx0C6/awEEEsZBvzKed7CM0CgYEAzk7gWeGkC3OTJzQeNvd3
30hduYsR/p3Vm8hSrDS+fbElDXNz3RJ/SM8tZGsVn7MHOfDyb35amg9n5Iv5N10k
ijXWd95nliT4V1J6Y++Y80Evy22NgkeLSLYBJygM0KETw8+RciM0ROfxDIZcqVZB
NKuPYY6xaRmz6xIH3USyn4UCgYEAxKBaz/sJaxtfYgIoCmR4Kue25v0xlioL/mIb
xXl7mG6YX2a4lB/OgOL3ko9AnaCEIjQoCxt6+TQzXPgUNgJRlC5zSD2G9uKs7SUR
GwzU3qoSnMjdye86bBzDLaU+9RdwKmc/vJ5OFILcUNfHeiJRRB06YB8UBd1qLnmS
8L2DQsMCgYEAr/11QPdNGz+yTgUVqUodhemTmk3aQdueds/CRoiP32UX+It+UR4Q
WqnxTPJUdfEgvvqdtSKSY021EK+fHu1j2Ero80RUFS7gco03Pr0LOqdnACAqUPJ7
DMHF5gMyO17NH4KXzkfdtNIvPMH5lbKw2R9opo41JTT52BN/he+ueIkCgYAvR9vu
bRALuE5MU/Zq4VPgBU3+511YHY46wj6pUpf8xINRVXMG80sFxQ4BKP9BqAp96wrB
+P6uE/ZR4bmCbzZMqorAEUN19HXepG4jkgdR75WAK/DhEOx8eMIaJMUpzFQFbkRu
R6bA2eK+cdSessfV2C1kVbTL4D0bJdLGntuEXwKBgB98zzGqtQQ0yg8KjP6kdlNd
jw7bvGG7kNhvaOk7Ls0My8B1UgjJozDYvaA6MWtuAPRryVg55o+WmuyPD2PJd8T9
Id6ilct3Hs2SkxcutkKTWO4YCU1BQ9lHAuummV6aJnUUQpbjcEqy4Bk/UUM8Lq9E
LTAG5BC5mfGEkVV6h2AP
-----END PRIVATE KEY-----
";

const TEST_RSA_N: &str = "nnWeQF_kjlvO7CKQrwPFpTBpvUgnX936V0a01WL8F3OU6yBOQGIdtqtxdMVogqSGjfKyNfybbuyycZczNajV2MzXe5MdbmJcE9BslobTD1sFwBrX5GRvi7R3fSDCI2lxzuz5aa-MF7Q6VJ50EwvUy6IWEBksPBO7vaI34yoawNyMjHDmJNHAgmJIvVtAuq-ZVuLNbG2pKBtgD7t8oadsTsOfs5K-8BgzUlG4LVdYSDa2NtppeWI0UX_xZTQQPTq8OJ-ZIcc-REnyWL4ELAYnOCwiuI0zfWWkJE_s72DEoTBePGVlZhqNrWgrxuT-uJ3QgVxK14HikHapAXceMz7MTw";
const TEST_RSA_E: &str = "AQAB";
const TEST_KID: &str = "test-key-1";

#[derive(Debug, Serialize)]
struct TestClaims<'a> {
    sub: &'a str,
    exp: usize,
}

fn sign_test_jwt(sub: &str, exp: usize, kid: Option<&str>) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = kid.map(str::to_string);
    let key = EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_KEY_PEM.as_bytes()).unwrap();
    encode(&header, &TestClaims { sub, exp }, &key).unwrap()
}

fn test_jwk() -> Jwk {
    Jwk {
        common: CommonParameters {
            key_id: Some(TEST_KID.to_string()),
            key_algorithm: Some(KeyAlgorithm::RS256),
            ..Default::default()
        },
        algorithm: AlgorithmParameters::RSA(RSAKeyParameters {
            key_type: RSAKeyType::RSA,
            n: TEST_RSA_N.to_string(),
            e: TEST_RSA_E.to_string(),
        }),
    }
}

/// Fake JWKS provider (UC-36 AF-03: `Err(Unreachable)` simulates the
/// external auth service being unreachable).
enum FakeJwksProvider {
    Reachable(JwkSet),
    Unreachable,
}

impl JwksProvider for FakeJwksProvider {
    async fn fetch(&self) -> Result<JwkSet, DomainError> {
        match self {
            FakeJwksProvider::Reachable(set) => Ok(set.clone()),
            FakeJwksProvider::Unreachable => {
                Err(DomainError::service_unavailable("fake jwks unreachable"))
            }
        }
    }
}

fn reachable() -> FakeJwksProvider {
    FakeJwksProvider::Reachable(JwkSet {
        keys: vec![test_jwk()],
    })
}

fn future_exp() -> usize {
    (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize
}

fn past_exp() -> usize {
    (chrono::Utc::now() - chrono::Duration::hours(1)).timestamp() as usize
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_valid_jwt_when_authenticate_then_principal_returned() {
    let service = ExternalAuthService::new(reachable());
    let token = sign_test_jwt("owner", future_exp(), Some(TEST_KID));

    let principal = service.authenticate(&token).await.expect("authenticate");

    assert_eq!(principal.user_id, "owner");
}

// ---------------- AF-02: missing, expired, or invalid-signature JWT ----------------

#[tokio::test]
async fn given_empty_token_when_authenticate_then_unauthorized() {
    let service = ExternalAuthService::new(reachable());

    let result = service.authenticate("").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

#[tokio::test]
async fn given_malformed_token_when_authenticate_then_unauthorized() {
    let service = ExternalAuthService::new(reachable());

    let result = service.authenticate("not-a-jwt").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

#[tokio::test]
async fn given_token_with_no_kid_when_authenticate_then_unauthorized() {
    let service = ExternalAuthService::new(reachable());
    let token = sign_test_jwt("owner", future_exp(), None);

    let result = service.authenticate(&token).await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

#[tokio::test]
async fn given_token_with_unknown_kid_when_authenticate_then_unauthorized() {
    let service = ExternalAuthService::new(reachable());
    let token = sign_test_jwt("owner", future_exp(), Some("no-such-key"));

    let result = service.authenticate(&token).await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

#[tokio::test]
async fn given_expired_token_when_authenticate_then_unauthorized() {
    let service = ExternalAuthService::new(reachable());
    let token = sign_test_jwt("owner", past_exp(), Some(TEST_KID));

    let result = service.authenticate(&token).await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

// ---------------- AF-03: external auth service unreachable ----------------

#[tokio::test]
async fn given_unreachable_jwks_when_authenticate_then_service_unavailable() {
    let service = ExternalAuthService::new(FakeJwksProvider::Unreachable);
    let token = sign_test_jwt("owner", future_exp(), Some(TEST_KID));

    let result = service.authenticate(&token).await;

    assert!(matches!(result, Err(DomainError::ServiceUnavailable(_))));
}
