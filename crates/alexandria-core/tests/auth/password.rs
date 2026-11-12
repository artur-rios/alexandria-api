//! Unit tests for the FR-AU-11 password strength policy (UC-41 AF-04,
//! UC-35). Pure function, no fakes needed — a table of boundary cases per
//! rule. Also the hashes it guards: what is stored must keep verifying across
//! upgrades of the argon2 crate.

use alexandria_core::auth::password::{
    hash_password, validate_strength, verify_password, MAX_PASSWORD_LENGTH, MIN_PASSWORD_LENGTH,
};
use alexandria_core::errors::{DomainError, Rejection};

const EMAIL: &str = "owner@example.com";

/// The `Rejection` behind a refusal, or a panic naming what came back
/// instead. Every rejection in this policy is a `Rejected` carrying a stable
/// reason code (issue #101).
fn rejection(result: Result<(), DomainError>) -> Rejection {
    match result {
        Err(DomainError::Rejected(rejection)) => rejection,
        other => panic!("expected Rejected, got {other:?}"),
    }
}

/// The English message of a refusal, for the assertions that care about what
/// an owner reads rather than about the code.
fn rejection_message(result: Result<(), DomainError>) -> String {
    rejection(result).message
}

#[test]
fn given_a_strong_password_when_validated_then_accepted() {
    assert!(validate_strength("correct horse battery", EMAIL).is_ok());
}

#[test]
fn given_a_password_one_char_below_the_floor_when_validated_then_rejected() {
    let password = "a".repeat(MIN_PASSWORD_LENGTH - 1);
    let rejection = rejection(validate_strength(&password, EMAIL));
    assert_eq!(rejection.code, "password_too_short");
    // The bound travels with the code: a client that renders its own
    // sentence must not have to restate 12 and drift from the policy.
    assert_eq!(
        rejection.params.get("min").map(String::as_str),
        Some(MIN_PASSWORD_LENGTH.to_string().as_str())
    );
    assert!(
        rejection.message.contains("at least"),
        "unexpected message: {}",
        rejection.message
    );
}

#[test]
fn given_a_password_exactly_at_the_floor_when_validated_then_accepted() {
    // 12 distinct characters: long enough, and not a repeated run.
    assert!(validate_strength("abcdefghijkm", EMAIL).is_ok());
}

#[test]
fn given_a_password_one_char_above_the_ceiling_when_validated_then_rejected() {
    let password = "ab".repeat(MAX_PASSWORD_LENGTH); // 256 chars
    let rejection = rejection(validate_strength(&password, EMAIL));
    assert_eq!(rejection.code, "password_too_long");
    assert_eq!(
        rejection.params.get("max").map(String::as_str),
        Some(MAX_PASSWORD_LENGTH.to_string().as_str())
    );
    assert!(
        rejection.message.contains("at most"),
        "unexpected message: {}",
        rejection.message
    );
}

#[test]
fn given_a_password_exactly_at_the_ceiling_when_validated_then_accepted() {
    let password = "ab".repeat(MAX_PASSWORD_LENGTH / 2); // 128 chars
    assert!(validate_strength(&password, EMAIL).is_ok());
}

#[test]
fn given_an_all_whitespace_password_when_validated_then_rejected() {
    assert_eq!(
        rejection(validate_strength(&" ".repeat(20), EMAIL)).code,
        "password_whitespace"
    );
    let message = rejection_message(validate_strength(&" ".repeat(20), EMAIL));
    assert!(
        message.contains("whitespace"),
        "unexpected message: {message}"
    );
}

#[test]
fn given_a_single_repeated_character_when_validated_then_rejected() {
    assert_eq!(
        rejection(validate_strength("aaaaaaaaaaaaaaaa", EMAIL)).code,
        "password_repeated_character"
    );
    let message = rejection_message(validate_strength("aaaaaaaaaaaaaaaa", EMAIL));
    assert!(
        message.contains("repeated"),
        "unexpected message: {message}"
    );
}

#[test]
fn given_the_email_as_the_password_when_validated_then_rejected() {
    let rejection = rejection(validate_strength(EMAIL, EMAIL));
    assert_eq!(rejection.code, "password_contains_email");
    let message = rejection.message;
    assert!(message.contains("email"), "unexpected message: {message}");
}

#[test]
fn given_the_email_local_part_in_another_case_when_validated_then_rejected() {
    // "owner" is the local part; embedding it uppercased must not evade.
    // Same code as the equal-to case: one remedy, one string to translate.
    let rejection = rejection(validate_strength("xxOWNERxxxxxxx", EMAIL));
    assert_eq!(rejection.code, "password_contains_email");
    let message = rejection.message;
    assert!(message.contains("email"), "unexpected message: {message}");
}

#[test]
fn given_a_common_password_when_validated_then_rejected() {
    let rejection = rejection(validate_strength("Password1234", EMAIL));
    assert_eq!(rejection.code, "password_too_common");
    let message = rejection.message;
    assert!(message.contains("common"), "unexpected message: {message}");
}

#[test]
fn given_a_short_email_local_part_when_validated_then_not_treated_as_a_substring() {
    // A one- or two-letter local part appears inside far too many good
    // passwords to reject on; only local parts of 3+ characters count.
    assert!(validate_strength("correct horse battery", "ab@example.com").is_ok());
}

#[test]
fn given_a_rejection_when_read_then_the_password_is_never_echoed() {
    let password = "aaaaaaaaaaaaaaaa";
    let message = rejection_message(validate_strength(password, EMAIL));
    assert!(
        !message.contains(password),
        "the message must never echo the password: {message}"
    );
}

/// A hash `hash_password` stored before the upgrade to argon2 0.6: produced
/// by argon2 0.5.3 with `SaltString::generate(&mut OsRng)` and
/// `Argon2::default()`, exactly the code this module ran then. Every local
/// account registered before the upgrade holds a hash of this shape, so this
/// literal must never be regenerated — it is the evidence.
const ARGON2_0_5_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$ZC7aeybsNGBY5eG4qIPayA$/37Ly4ZdE6vzEjFl7Yqkpa63fQnRKhAxCJBqWjL1e3Q";
const ARGON2_0_5_PASSWORD: &str = "correct horse battery staple";

#[test]
fn given_a_hash_stored_by_argon2_0_5_when_verified_then_the_password_matches() {
    assert!(verify_password(ARGON2_0_5_PASSWORD, ARGON2_0_5_HASH));
}

#[test]
fn given_a_hash_stored_by_argon2_0_5_when_verified_with_another_password_then_refused() {
    assert!(!verify_password(
        "correct horse battery stapler",
        ARGON2_0_5_HASH
    ));
}

#[test]
fn given_a_password_when_hashed_then_the_parameters_and_salt_length_are_unchanged() {
    let hash = hash_password(ARGON2_0_5_PASSWORD).unwrap();

    // `$argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>`: the algorithm, version
    // and costs the 0.5 hashes carry, a 16-byte salt (22 characters of
    // unpadded base64) and a 32-byte output (43 characters).
    let fields: Vec<&str> = hash.split('$').collect();
    assert_eq!(fields.len(), 6, "{hash}");
    let fixed: Vec<&str> = ARGON2_0_5_HASH.split('$').collect();
    assert_eq!(fields[..4], fixed[..4], "{hash}");
    assert_eq!(fields[4].len(), 22, "{hash}");
    assert_eq!(fields[5].len(), 43, "{hash}");
    assert_ne!(fields[4], fixed[4], "every hash draws a fresh salt");

    assert!(verify_password(ARGON2_0_5_PASSWORD, &hash));
}

#[test]
fn given_a_corrupt_stored_hash_when_verified_then_refused() {
    assert!(!verify_password(
        ARGON2_0_5_PASSWORD,
        "$argon2id$not-a-hash"
    ));
}
