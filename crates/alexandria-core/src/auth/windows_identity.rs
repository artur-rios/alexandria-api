//! Who this process is running as (UC-45 / FR-AU-20, FR-AU-21).
//!
//! The only platform-conditional code in the workspace. It is kept behind a
//! trait so that everything built on it — the comparison, the startup gate,
//! the login handler — is testable on any platform against a fake, leaving
//! exactly one function that only Windows can exercise.
//!
//! See `docs/superpowers/specs/2026-08-18-windows-credential-login-design.md`.

use crate::errors::DomainError;

/// The account this process runs as.
pub trait WindowsIdentity: Send + Sync {
    /// The SID of that account, in the conventional string form
    /// (`S-1-5-21-…`). `Err` when the platform cannot answer — which is every
    /// non-Windows platform, and a Windows one whose token cannot be read.
    fn current_sid(&self) -> Result<String, DomainError>;
}

/// Reads the SID from the running process's access token.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessWindowsIdentity;

/// Whether the process runs as the account the configuration names
/// (FR-AU-21).
///
/// Compared case-insensitively and with surrounding whitespace trimmed: the
/// string form of a SID is conventionally upper-case, but an operator who
/// pasted a lower-cased one from some tool has not named a different account,
/// and failing them over it would be a puzzle with no lesson in it.
pub fn verify_owner(
    identity: &impl WindowsIdentity,
    configured_sid: &str,
) -> Result<(), DomainError> {
    let actual = identity.current_sid()?;
    let expected = configured_sid.trim();

    if actual.trim().eq_ignore_ascii_case(expected) {
        return Ok(());
    }

    // Both values are named because a mismatch is the operator's to diagnose,
    // and a SID identifies an account rather than authenticating one — there
    // is nothing here to leak.
    Err(DomainError::Config(format!(
        "this process runs as {actual}, but auth.windows_owner_sid names {expected}. \
         Windows mode authenticates by the account the process runs as, so it refuses \
         to start as any other."
    )))
}

#[cfg(windows)]
// The Win32 token API is raw FFI with no safe wrapper; the exception is
// scoped to this one function, in the same spirit as `alexandria-ffi`'s
// `#[allow(unsafe_code)]` on its `#[no_mangle]` exports.
#[allow(unsafe_code)]
impl WindowsIdentity for ProcessWindowsIdentity {
    fn current_sid(&self) -> Result<String, DomainError> {
        use std::ptr;

        use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
        use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
        use windows_sys::Win32::Security::{
            GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        // Every call below is `unsafe` because it is raw Win32. The scope is
        // deliberately this one function: nothing above it in this file, and
        // nothing that uses it, needs `unsafe` at all.
        unsafe {
            let mut token: HANDLE = ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(DomainError::Config(
                    "could not open this process's access token to read its account".to_string(),
                ));
            }

            // Asking with a zero-length buffer is how Win32 reports the size
            // it wants; it always "fails", and the length is the answer.
            let mut needed: u32 = 0;
            GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut needed);
            if needed == 0 {
                CloseHandle(token);
                return Err(DomainError::Config(
                    "could not size this process's token information".to_string(),
                ));
            }

            let mut buffer = vec![0u8; needed as usize];
            if GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            ) == 0
            {
                CloseHandle(token);
                return Err(DomainError::Config(
                    "could not read this process's token information".to_string(),
                ));
            }
            CloseHandle(token);

            let token_user = &*(buffer.as_ptr() as *const TOKEN_USER);
            let mut raw: *mut u16 = ptr::null_mut();
            if ConvertSidToStringSidW(token_user.User.Sid, &mut raw) == 0 {
                return Err(DomainError::Config(
                    "could not convert this process's account SID to text".to_string(),
                ));
            }

            let mut len = 0usize;
            while *raw.add(len) != 0 {
                len += 1;
            }
            let sid = String::from_utf16_lossy(std::slice::from_raw_parts(raw, len));
            LocalFree(raw.cast());

            Ok(sid)
        }
    }
}

#[cfg(not(windows))]
impl WindowsIdentity for ProcessWindowsIdentity {
    /// Windows mode cannot work anywhere else, and saying so at startup is far
    /// kinder than an authentication failure with no explanation.
    fn current_sid(&self) -> Result<String, DomainError> {
        Err(DomainError::Config(
            "auth.mode is \"windows\", but this build is not running on Windows: \
             the mode authenticates by the Windows account this process runs as"
                .to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Holds the failure as a `String` rather than a `DomainError`, because
    /// `DomainError` is deliberately not `Clone` and a test's convenience is
    /// no reason to make it so.
    struct FakeIdentity {
        sid: Option<String>,
        failure: Option<String>,
    }

    impl WindowsIdentity for FakeIdentity {
        fn current_sid(&self) -> Result<String, DomainError> {
            match (&self.sid, &self.failure) {
                (Some(sid), _) => Ok(sid.clone()),
                (None, Some(message)) => Err(DomainError::Config(message.clone())),
                (None, None) => unreachable!("fake configured with neither outcome"),
            }
        }
    }

    fn reporting(sid: &str) -> FakeIdentity {
        FakeIdentity {
            sid: Some(sid.to_string()),
            failure: None,
        }
    }

    const OWNER: &str = "S-1-5-21-1004336348-1177238915-682003330-1001";
    const OTHER: &str = "S-1-5-21-1004336348-1177238915-682003330-1002";

    #[test]
    fn given_the_process_runs_as_the_configured_account_when_verified_then_ok() {
        assert!(verify_owner(&reporting(OWNER), OWNER).is_ok());
    }

    /// A mismatch is a configuration error the operator has to diagnose, and
    /// neither value is a secret — a SID is an identifier, not a credential —
    /// so the message names both.
    #[test]
    fn given_a_different_account_when_verified_then_error_names_both_sids() {
        let message = verify_owner(&reporting(OTHER), OWNER)
            .unwrap_err()
            .to_string();

        assert!(message.contains(OWNER), "{message}");
        assert!(message.contains(OTHER), "{message}");
    }

    /// Windows SIDs are compared case-insensitively: the string form is
    /// conventionally upper-case, but an operator pasting from a tool that
    /// lower-cases it has not configured a different account.
    #[test]
    fn given_the_configured_sid_differs_only_in_case_when_verified_then_ok() {
        assert!(verify_owner(&reporting(OWNER), &OWNER.to_lowercase()).is_ok());
    }

    #[test]
    fn given_surrounding_whitespace_in_the_configured_sid_when_verified_then_ok() {
        assert!(verify_owner(&reporting(OWNER), &format!("  {OWNER}  ")).is_ok());
    }

    /// The non-Windows stub's failure must reach the operator as-is, not be
    /// reshaped into a mismatch they will chase.
    #[test]
    fn given_the_sid_cannot_be_read_when_verified_then_that_error_propagates() {
        let identity = FakeIdentity {
            sid: None,
            failure: Some("no token here".to_string()),
        };

        let message = verify_owner(&identity, OWNER).unwrap_err().to_string();

        assert!(message.contains("no token here"), "{message}");
    }

    /// The one thing that cannot be tested anywhere else. Asserts shape, not a
    /// value: the SID differs per machine.
    #[cfg(windows)]
    #[test]
    fn given_a_real_windows_process_when_its_sid_is_read_then_it_is_well_formed() {
        let sid = ProcessWindowsIdentity.current_sid().unwrap();

        assert!(sid.starts_with("S-1-"), "{sid}");
        assert!(!sid.contains('\0'), "{sid} contains an interior NUL");
        assert!(
            sid.len() > "S-1-5-21-".len(),
            "{sid} is too short to be a SID"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn given_a_non_windows_platform_when_the_sid_is_read_then_it_fails_naming_the_platform() {
        let message = ProcessWindowsIdentity
            .current_sid()
            .unwrap_err()
            .to_string();

        assert!(message.to_lowercase().contains("windows"), "{message}");
    }
}
