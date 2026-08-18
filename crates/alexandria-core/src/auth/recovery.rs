//! Recovery codes: the only way back into a local account whose password has
//! been forgotten (FR-AU-13 … FR-AU-19).
//!
//! Pure functions over one credential's representation, in the same shape as
//! `password.rs` — no repository, no clock — so the format and the hash are
//! testable without a database.

use argon2::password_hash::rand_core::{OsRng, RngCore};

use crate::catalog::fs::sha256_hex;

/// How many codes an owner holds at once. Ten, because a recovery method that
/// runs out on the second mistake is not one.
pub const RECOVERY_CODE_COUNT: usize = 10;

/// Characters per code, excluding the presentational hyphen. Ten characters
/// of a 32-symbol alphabet is fifty bits — far past guessing, and still short
/// enough to write on paper.
pub const RECOVERY_CODE_LENGTH: usize = 10;

/// Crockford base32: the digits and upper-case letters minus `I`, `L`, `O`
/// and `U`, so no character on a printed list can be mistaken for another.
/// Exactly 32 symbols, so masking a random byte to five bits is uniform —
/// 256 divides by 32, leaving no modulo bias to correct for.
const RECOVERY_CODE_ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// How many characters precede the hyphen when a code is displayed.
const RECOVERY_CODE_GROUP: usize = 5;

/// Generate a fresh set of `RECOVERY_CODE_COUNT` codes, formatted for display.
///
/// Distinctness is not enforced by a retry loop: at fifty bits each, a
/// collision within ten draws is far less likely than the disk failing
/// mid-write, and a loop would add a branch nothing can ever exercise.
pub fn generate_recovery_codes() -> Vec<String> {
    (0..RECOVERY_CODE_COUNT).map(|_| generate_one()).collect()
}

fn generate_one() -> String {
    let mut bytes = [0u8; RECOVERY_CODE_LENGTH];
    OsRng.fill_bytes(&mut bytes);

    let mut code = String::with_capacity(RECOVERY_CODE_LENGTH + 1);
    for (index, byte) in bytes.iter().enumerate() {
        if index == RECOVERY_CODE_GROUP {
            code.push('-');
        }
        code.push(RECOVERY_CODE_ALPHABET[usize::from(byte & 0x1f)] as char);
    }
    code
}

/// Reduce a code as typed to the canonical form that was hashed.
///
/// Upper-cases and drops everything that is not a letter or a digit, so the
/// hyphen a client printed, the case the owner used, and any spaces they
/// typed cannot decide whether their code works.
pub fn normalize_recovery_code(input: &str) -> String {
    input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// Hash a code the way it is stored: normalized, then SHA-256.
///
/// SHA-256 rather than Argon2 for the reason `tokens.rs` gave before it:
/// Argon2's work factor exists to slow the guessing of a secret a person
/// chose, and this is fifty bits chosen by the operating system. The search
/// space already does that job; a slow hash would only be a cost paid on
/// every lookup. Storing the hash rather than the code still matters, and for
/// FR-AU-06's reason — a database read must not yield a working credential.
pub fn hash_recovery_code(input: &str) -> String {
    sha256_hex(normalize_recovery_code(input).as_bytes())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn given_generation_when_called_then_ten_distinct_codes() {
        let codes = generate_recovery_codes();

        assert_eq!(codes.len(), RECOVERY_CODE_COUNT);
        let distinct: HashSet<&String> = codes.iter().collect();
        assert_eq!(
            distinct.len(),
            RECOVERY_CODE_COUNT,
            "codes repeated: {codes:?}"
        );
    }

    /// The alphabet excludes I, L, O and U so nothing on a printed list is
    /// ambiguous, and the hyphen is presentation only.
    #[test]
    fn given_a_generated_code_when_inspected_then_it_is_two_groups_of_five_crockford_base32() {
        for code in generate_recovery_codes() {
            let (left, right) = code.split_once('-').expect("no hyphen in {code}");
            assert_eq!(left.len(), 5, "{code}");
            assert_eq!(right.len(), 5, "{code}");
            for character in code.chars().filter(|c| *c != '-') {
                assert!(
                    RECOVERY_CODE_ALPHABET.contains(&(character as u8)),
                    "{character} in {code} is not in the alphabet"
                );
            }
        }
    }

    /// A code is typed off paper, so the hyphen, the case, and stray spaces
    /// must not decide whether it works.
    #[test]
    fn given_the_same_code_written_four_ways_when_normalized_then_all_agree() {
        let spellings = [
            "ABCDE-FGHJK",
            "abcde-fghjk",
            "ABCDEFGHJK",
            "  abcde fghjk  ",
        ];

        let normalized: HashSet<String> = spellings
            .iter()
            .map(|s| normalize_recovery_code(s))
            .collect();

        assert_eq!(normalized.len(), 1, "{normalized:?}");
        assert_eq!(normalized.into_iter().next().unwrap(), "ABCDEFGHJK");
    }

    #[test]
    fn given_the_same_code_written_four_ways_when_hashed_then_all_agree() {
        let hashes: HashSet<String> = ["ABCDE-FGHJK", "abcde-fghjk", "ABCDEFGHJK", " abcde fghjk "]
            .iter()
            .map(|s| hash_recovery_code(s))
            .collect();

        assert_eq!(hashes.len(), 1);
    }

    #[test]
    fn given_two_different_codes_when_hashed_then_hashes_differ() {
        assert_ne!(
            hash_recovery_code("ABCDE-FGHJK"),
            hash_recovery_code("ABCDE-FGHJM")
        );
    }

    /// The stored value must not be reversible to the code, and must not
    /// simply *be* the code.
    #[test]
    fn given_a_code_when_hashed_then_the_hash_is_sha256_hex_and_not_the_code() {
        let hash = hash_recovery_code("ABCDE-FGHJK");

        assert_eq!(hash.len(), 64, "{hash}");
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!hash.contains("ABCDE"));
    }
}
