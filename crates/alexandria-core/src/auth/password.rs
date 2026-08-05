use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;

use crate::errors::DomainError;

/// Salt and hash `password` with Argon2 (UC-35 / FR-AU-05, FR-AU-06). The
/// plaintext is never stored — only this hash is persisted.
pub fn hash_password(password: &str) -> Result<String, DomainError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|err| DomainError::internal(format!("password hashing failed: {err}")))
}

/// Verify `password` against a previously stored Argon2 hash (UC-34 /
/// FR-AU-04). `false` for a wrong password as well as for a corrupt stored
/// hash — either way the caller is denied, never told which (AF-02: "no
/// plaintext is logged", and the outcome must not distinguish the two).
pub fn verify_password(password: &str, stored_hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored_hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}
