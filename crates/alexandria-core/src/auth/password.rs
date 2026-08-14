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

/// The password length floor (FR-AU-11, UC-41 AF-04). Length is the only
/// strength lever that scales; character-class rules ("one digit, one
/// symbol") push people toward predictable substitutions without adding
/// much real entropy, so this policy asks for length instead.
pub const MIN_PASSWORD_LENGTH: usize = 12;

/// The password length ceiling (FR-AU-11). Argon2 pays its cost per byte,
/// so an unbounded password is a cheap way to make the server work hard on
/// an endpoint that is deliberately reachable without authentication.
pub const MAX_PASSWORD_LENGTH: usize = 128;

/// Passwords common enough to be guessed early, compared case-insensitively.
///
/// Every entry is at least `MIN_PASSWORD_LENGTH` characters: anything
/// shorter is already unreachable through the length rule, so a shorter
/// entry here would be dead weight. Deliberately small — a real corpus
/// check needs a downloaded breach dataset, which the Technology Stack
/// Document's dependency discipline rules out. This is the place to add
/// long passwords that become common.
const COMMON_PASSWORDS: &[&str] = &[
    "password1234",
    "passwordpassword",
    "password123456",
    "123456789012",
    "1234567890123",
    "12345678901234",
    "qwertyuiop123",
    "qwerty1234567",
    "qwertyuiopasd",
    "administrator",
    "iloveyou1234",
    "letmein12345",
    "welcome123456",
    "changeme1234",
    "abcdefghijkl",
    "monkey1234567",
    "dragon1234567",
    "superman1234",
    "princess1234",
    "football1234",
    "baseball1234",
    "sunshine1234",
    "michael123456",
    "shadow1234567",
    "alexandria12",
];

/// The shortest email local part treated as a forbidden substring. A one-
/// or two-character local part ("jo@…") appears inside a great many
/// perfectly good passwords; rejecting on it would be noise, not security.
const MIN_LOCAL_PART_FOR_SUBSTRING_CHECK: usize = 3;

/// Check `password` against the strength policy (FR-AU-11), used by both
/// UC-41 registration and UC-35 change. `email` is the address submitted in
/// the same request — the first thing an attacker tries, so it must not be
/// the password or appear inside it.
///
/// Every rejection is a `Rejected` naming the unmet rule twice over: an
/// English message a log or a code-unaware client shows, and a stable code
/// plus the bound that was violated, which is the only form a client
/// translating into another language can use (issue #101). The message never
/// echoes the password (FR-AU-06).
pub fn validate_strength(password: &str, email: &str) -> Result<(), DomainError> {
    let length = password.chars().count();
    if length < MIN_PASSWORD_LENGTH {
        return Err(DomainError::rejected(
            "password_too_short",
            format!("password must be at least {MIN_PASSWORD_LENGTH} characters"),
        )
        .with_param("min", MIN_PASSWORD_LENGTH.to_string()));
    }
    if length > MAX_PASSWORD_LENGTH {
        return Err(DomainError::rejected(
            "password_too_long",
            format!("password must be at most {MAX_PASSWORD_LENGTH} characters"),
        )
        .with_param("max", MAX_PASSWORD_LENGTH.to_string()));
    }

    if password.trim().is_empty() {
        return Err(DomainError::rejected(
            "password_whitespace",
            "password must not be entirely whitespace",
        ));
    }

    // A single character repeated passes any length floor. Checked on
    // characters, not bytes, so a repeated multi-byte character counts too.
    let mut chars = password.chars();
    let first = chars
        .next()
        .expect("non-empty: length >= MIN_PASSWORD_LENGTH");
    if chars.all(|c| c == first) {
        return Err(DomainError::rejected(
            "password_repeated_character",
            "password must not be a single repeated character",
        ));
    }

    let lowered = password.to_lowercase();
    if COMMON_PASSWORDS.contains(&lowered.as_str()) {
        return Err(DomainError::rejected(
            "password_too_common",
            "password is too common; choose a less predictable one",
        ));
    }

    // Both shapes below share one code: the remedy an owner needs — pick a
    // password that has nothing to do with the address — is identical, and a
    // second code would be a second string for every client to translate for
    // no difference the owner can see. The English message still tells the
    // two apart, for a log.
    let email_lowered = email.to_lowercase();
    if !email_lowered.is_empty() && lowered == email_lowered {
        return Err(DomainError::rejected(
            "password_contains_email",
            "password must not be the email address",
        ));
    }
    let local_part = email_lowered.split('@').next().unwrap_or_default();
    if local_part.chars().count() >= MIN_LOCAL_PART_FOR_SUBSTRING_CHECK
        && lowered.contains(local_part)
    {
        return Err(DomainError::rejected(
            "password_contains_email",
            "password must not contain the email address",
        ));
    }

    Ok(())
}
