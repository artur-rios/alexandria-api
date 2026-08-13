//! Unit tests for the FR-AU-11 password strength policy (UC-41 AF-04,
//! UC-35). Pure function, no fakes needed — a table of boundary cases per
//! rule.

use alexandria_core::auth::password::{
    validate_strength, MAX_PASSWORD_LENGTH, MIN_PASSWORD_LENGTH,
};
use alexandria_core::errors::DomainError;

const EMAIL: &str = "owner@example.com";

/// The message of an `InvalidInput`, or a panic naming what came back
/// instead. Every rejection in this policy is an `InvalidInput`.
fn rejection_message(result: Result<(), DomainError>) -> String {
    match result {
        Err(DomainError::InvalidInput(message)) => message,
        other => panic!("expected InvalidInput, got {other:?}"),
    }
}

#[test]
fn given_a_strong_password_when_validated_then_accepted() {
    assert!(validate_strength("correct horse battery", EMAIL).is_ok());
}

#[test]
fn given_a_password_one_char_below_the_floor_when_validated_then_rejected() {
    let password = "a".repeat(MIN_PASSWORD_LENGTH - 1);
    let message = rejection_message(validate_strength(&password, EMAIL));
    assert!(
        message.contains("at least"),
        "unexpected message: {message}"
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
    let message = rejection_message(validate_strength(&password, EMAIL));
    assert!(message.contains("at most"), "unexpected message: {message}");
}

#[test]
fn given_a_password_exactly_at_the_ceiling_when_validated_then_accepted() {
    let password = "ab".repeat(MAX_PASSWORD_LENGTH / 2); // 128 chars
    assert!(validate_strength(&password, EMAIL).is_ok());
}

#[test]
fn given_an_all_whitespace_password_when_validated_then_rejected() {
    let message = rejection_message(validate_strength(&" ".repeat(20), EMAIL));
    assert!(
        message.contains("whitespace"),
        "unexpected message: {message}"
    );
}

#[test]
fn given_a_single_repeated_character_when_validated_then_rejected() {
    let message = rejection_message(validate_strength("aaaaaaaaaaaaaaaa", EMAIL));
    assert!(
        message.contains("repeated"),
        "unexpected message: {message}"
    );
}

#[test]
fn given_the_email_as_the_password_when_validated_then_rejected() {
    let message = rejection_message(validate_strength(EMAIL, EMAIL));
    assert!(message.contains("email"), "unexpected message: {message}");
}

#[test]
fn given_the_email_local_part_in_another_case_when_validated_then_rejected() {
    // "owner" is the local part; embedding it uppercased must not evade.
    let message = rejection_message(validate_strength("xxOWNERxxxxxxx", EMAIL));
    assert!(message.contains("email"), "unexpected message: {message}");
}

#[test]
fn given_a_common_password_when_validated_then_rejected() {
    let message = rejection_message(validate_strength("Password1234", EMAIL));
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
