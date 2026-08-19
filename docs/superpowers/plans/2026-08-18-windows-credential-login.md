# Windows Credential Login Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a third authentication mode in which the Windows account the server process runs as is the credential, verified once at startup against a configured SID.

**Architecture:** A `WindowsIdentity` port with a `#[cfg(windows)]` implementation and a non-Windows stub, so every decision around it is testable on any platform against a fake. Session handling is reused verbatim from local mode — `RuntimeAuthService::Windows` wraps the same `LocalAuthService` — so only the mode value differs.

**Tech Stack:** Rust 2021, `windows-sys` (Windows targets only), `axum` 0.8, `sqlx` (SQLite, for the existing session store), `serde`.

## Global Constraints

- **Design spec:** [`docs/superpowers/specs/2026-08-18-windows-credential-login-design.md`](../specs/2026-08-18-windows-credential-login-design.md). Every decision traces to it; do not improvise past it.
- **Exactly one mode is active at runtime.** `AuthMode` gains `Windows`; it is mutually exclusive with `Local` and `External` (FR-AU-01, BR-17).
- **The SID check runs once at startup, never per request.** A process cannot change the account it runs as.
- **`windows-sys` is declared only under `[target.'cfg(windows)'.dependencies]`** — it must never enter the dependency graph on Linux or macOS.
- **`alexandria-core` must keep compiling and passing its tests on non-Windows.** Everything except one `#[cfg(windows)]` test is generic over the `WindowsIdentity` trait and runs everywhere.
- **No new machine-readable reason codes.** Windows login takes no input, so there is no input to reject.
- Test naming: `given_<condition>_when_<action>_then_<outcome>`.
- Handler and repository tests are integration tests under `crates/alexandria-core/tests/`, pulled into `tests/auth.rs` with `#[path = "auth/<file>.rs"] mod <file>;`. Shared fakes live in `tests/common/mod.rs`. No command source file carries a `#[cfg(test)]` module.
- `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --check` must both be clean before every commit.
- Commit subjects: lowercase Conventional Commits, ≤50 chars, imperative.
- **Do not run `cargo test --workspace`** — it links FFmpeg and exceeds a subagent's tool timeout. Iterate with targeted runs and `cargo build --workspace --all-targets`; the controller runs the full suite.

---

### Task 1: The mode and its configuration

**Files:**
- Modify: `crates/alexandria-core/src/config.rs`
- Modify: `crates/alexandria-core/tests/config.rs`
- Modify: `config.toml.example`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `AuthMode::Windows`, with `AuthMode::as_str` returning `"windows"` and the TOML value `"windows"`
  - `AuthSettings.windows_owner_sid: String`, with the `ALEXANDRIA_AUTH_WINDOWS_OWNER_SID` override
  - `AuthSettings::validate` refusing Windows mode with an empty `windows_owner_sid`

- [ ] **Step 1: Write the failing tests**

Append to `crates/alexandria-core/tests/config.rs` (the file already imports
`AuthMode`, `AuthSettings`, `Secret` and `Settings`):

```rust
#[test]
fn given_windows_mode_when_parsed_then_mode_and_sid_round_trip() {
    let toml = r#"
[auth]
mode = "windows"
windows_owner_sid = "S-1-5-21-1004336348-1177238915-682003330-1001"
"#;
    let settings: Settings = toml::from_str(toml).unwrap();

    assert_eq!(settings.auth.mode, AuthMode::Windows);
    assert_eq!(settings.auth.mode.as_str(), "windows");
    assert_eq!(
        settings.auth.windows_owner_sid,
        "S-1-5-21-1004336348-1177238915-682003330-1001"
    );
}

/// A mode that names no account is a mode with no authentication at all: a
/// process started as the wrong account — SYSTEM, say — would serve the
/// catalog anyway.
#[test]
fn given_windows_mode_without_a_sid_when_validated_then_error_names_the_key() {
    let auth = AuthSettings {
        mode: AuthMode::Windows,
        ..AuthSettings::default()
    };

    let message = auth.validate().unwrap_err().to_string();

    assert!(message.contains("auth.windows_owner_sid"), "{message}");
}

#[test]
fn given_windows_mode_with_a_sid_when_validated_then_ok() {
    let auth = AuthSettings {
        mode: AuthMode::Windows,
        windows_owner_sid: "S-1-5-21-1-2-3-1001".to_string(),
        ..AuthSettings::default()
    };

    assert!(auth.validate().is_ok());
}

/// The other two modes must not start demanding a SID.
#[test]
fn given_local_or_external_mode_when_validated_then_the_sid_is_not_required() {
    let local = AuthSettings {
        mode: AuthMode::Local,
        ..AuthSettings::default()
    };
    assert!(local.validate().is_ok());

    let external = AuthSettings {
        mode: AuthMode::External,
        heimdall_token_secret: Secret::new("secret"),
        heimdall_scope_id: "0b8d3a6e-4a1f-4c2b-9f1e-7c5d2a9b3e40".to_string(),
        ..AuthSettings::default()
    };
    assert!(external.validate().is_ok());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p alexandria-core --test config`
Expected: FAIL to compile — `no variant named Windows found for enum AuthMode`.

- [ ] **Step 3: Add the mode**

In `crates/alexandria-core/src/config.rs`, add the variant to `AuthMode` and its
string:

```rust
pub enum AuthMode {
    External,
    Local,
    /// The Windows account this process runs as is the credential (UC-45 /
    /// FR-AU-20). Mutually exclusive with the other two: exactly one mode is
    /// active at runtime (FR-AU-01).
    Windows,
}
```

and in `as_str`:

```rust
            AuthMode::Windows => "windows",
```

`#[serde(rename_all = "lowercase")]` is already on the enum, so `"windows"`
deserializes from a config file with no further change.

There **is** a `match_mode` helper (`config.rs:456`) backing the
`ALEXANDRIA_AUTH_MODE` environment override, and it has its own unit tests
around line 481 — verified, not assumed. Add a `"windows"` arm to it and a case
to those tests, or the mode will be settable from the file but silently ignored
from the environment.

- [ ] **Step 4: Add the setting**

Add to `AuthSettings`, after the Heimdall keys:

```rust
    /// Windows mode only: the SID of the account this process must run as,
    /// e.g. `S-1-5-21-1004336348-1177238915-682003330-1001`.
    ///
    /// A SID rather than a username because usernames are renameable and
    /// reusable, while a SID is neither. Not a secret — an identifier — so it
    /// is a plain `String` rather than a `Secret`.
    #[serde(default)]
    pub windows_owner_sid: String,
```

Add `windows_owner_sid: String::new(),` to the `Default` impl, and the override
alongside the other `ALEXANDRIA_AUTH_*` blocks in `apply_env_overrides`:

```rust
        if let Ok(sid) = env::var("ALEXANDRIA_AUTH_WINDOWS_OWNER_SID") {
            self.auth.windows_owner_sid = sid;
        }
```

- [ ] **Step 5: Extend `validate`**

`AuthSettings::validate` currently returns early for anything that is not
`AuthMode::External`. Restructure it into a `match` so each mode states its own
rule, and add the Windows arm:

```rust
        match self.mode {
            AuthMode::Local => Ok(()),
            AuthMode::External => { /* the existing secret and scope checks */ }
            AuthMode::Windows => {
                if self.windows_owner_sid.trim().is_empty() {
                    return Err(DomainError::Config(
                        "auth.windows_owner_sid is unset: Windows mode authenticates by the \
                         account this process runs as, so it must name the account it expects"
                            .to_string(),
                    ));
                }
                Ok(())
            }
        }
```

Keep the existing external-mode checks exactly as they are — only their
surrounding control flow moves.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p alexandria-core --test config`
Expected: PASS, all tests in the file.

- [ ] **Step 7: Document the key in the example config**

In `config.toml.example`, extend the `mode` comment to list all three values and
add the key:

```toml
# Windows mode only: the SID of the account this process must run as. Startup
# fails unless the process is running on Windows as this account. Find it with
# `whoami /user`. Not a secret — it is an identifier, not a credential.
#
# Read what this mode does and does not prove before enabling it: it proves the
# process was launched by this account, never who is calling, so any caller that
# can reach the port is authorized. Keep `http.bind_addr` on loopback.
windows_owner_sid = ""
```

- [ ] **Step 8: Check lints and formatting**

Run: `cargo clippy -p alexandria-core --all-targets -- -D warnings && cargo fmt --check`
Expected: clean. Note the new `AuthMode` variant may make other `match`
statements non-exhaustive — fix any that appear rather than adding a catch-all
arm, since an exhaustive match is what will flag the next mode.

- [ ] **Step 9: Commit**

```bash
git add crates/alexandria-core/src/config.rs crates/alexandria-core/tests/config.rs config.toml.example
git commit -m "feat: add the windows auth mode"
```

---

### Task 2: The Windows identity port

The only platform-conditional code in the workspace. One trait, one real
implementation, one stub, and the SID comparison — which is generic, so it is
tested on every platform.

**Files:**
- Create: `crates/alexandria-core/src/auth/windows_identity.rs`
- Modify: `crates/alexandria-core/src/auth/mod.rs`
- Modify: `crates/alexandria-core/Cargo.toml`
- Modify: `Cargo.toml` (workspace dependency declaration)

**Interfaces:**
- Consumes: `AuthMode::Windows`, `AuthSettings.windows_owner_sid` (Task 1).
- Produces, all in `crate::auth::windows_identity`:
  - `pub trait WindowsIdentity: Send + Sync { fn current_sid(&self) -> Result<String, DomainError>; }`
  - `pub struct ProcessWindowsIdentity;` implementing it — real under `#[cfg(windows)]`, always-failing stub under `#[cfg(not(windows))]`
  - `pub fn verify_owner(identity: &impl WindowsIdentity, configured_sid: &str) -> Result<(), DomainError>`

- [ ] **Step 1: Declare the dependency**

In the root `Cargo.toml`'s `[workspace.dependencies]`, add:

```toml
# Windows-only: reading this process's token to learn which account it runs as
# (UC-45). The thinnest binding available — raw FFI declarations, no wrapper
# layer — and it enters the graph only on Windows targets.
windows-sys = { version = "0.61", features = ["Win32_Foundation", "Win32_Security", "Win32_Security_Authorization", "Win32_System_Threading"] }
```

In `crates/alexandria-core/Cargo.toml`, add a target-conditional section after
`[dependencies]`:

```toml
[target.'cfg(windows)'.dependencies]
windows-sys.workspace = true
```

If the feature names above do not exist in the version cargo resolves, adjust
them to the ones that provide `OpenProcessToken`, `GetTokenInformation`,
`ConvertSidToStringSidW`, `LocalFree` and `CloseHandle`, and say so in your
report.

- [ ] **Step 2: Declare the module**

In `crates/alexandria-core/src/auth/mod.rs`, add `pub mod windows_identity;` to
the module list, keeping it alphabetical (after `pub mod recovery;`).

- [ ] **Step 3: Write the failing tests**

Create `crates/alexandria-core/src/auth/windows_identity.rs` containing **only**
this test module for now. These are pure-function tests over a trait fake, so
they live in the source file as `recovery.rs`'s do:

```rust
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
        assert!(sid.len() > "S-1-5-21-".len(), "{sid} is too short to be a SID");
    }

    #[cfg(not(windows))]
    #[test]
    fn given_a_non_windows_platform_when_the_sid_is_read_then_it_fails_naming_the_platform() {
        let message = ProcessWindowsIdentity.current_sid().unwrap_err().to_string();

        assert!(message.to_lowercase().contains("windows"), "{message}");
    }
}
```

`DomainError` derives only `Debug` and `Error` — verified, not assumed — which is
why the fake above holds its failure as a `String`. Do not add a `Clone` derive
to `DomainError` to simplify this.

- [ ] **Step 4: Run the tests to verify they fail**

Run: `cargo test -p alexandria-core windows_identity`
Expected: FAIL to compile — `cannot find trait WindowsIdentity in this scope`.

- [ ] **Step 5: Write the implementation**

Prepend to `crates/alexandria-core/src/auth/windows_identity.rs`:

```rust
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
impl WindowsIdentity for ProcessWindowsIdentity {
    fn current_sid(&self) -> Result<String, DomainError> {
        use std::ptr;

        use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
        use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
        use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
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
```

`crates/alexandria-core/src/lib.rs` carries `#![deny(unsafe_code)]` at the crate
root — verified, not assumed — so the `#[cfg(windows)]` impl above **will not
compile** without an opt-out. Put `#[allow(unsafe_code)]` on that `impl` block
only, with a comment saying the Win32 token API is raw FFI and the exception is
scoped to this one function, in the same spirit as `alexandria-ffi`'s
`#[allow(unsafe_code)]` on its `#[no_mangle]` exports. **Never** relax the
crate-level lint.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p alexandria-core windows_identity`
Expected: PASS — 6 tests, one of which is the platform-appropriate half of the
last pair.

- [ ] **Step 7: Confirm the non-Windows build**

Run: `cargo build -p alexandria-core --target x86_64-unknown-linux-gnu` if that
target is installed. If it is not, skip it and say so in your report — the
`#[cfg(not(windows))]` stub is covered by its unit test on a non-Windows CI, and
installing a cross-compilation target is out of this task's scope.

- [ ] **Step 8: Check lints and formatting**

Run: `cargo clippy -p alexandria-core --all-targets -- -D warnings && cargo fmt --check`
Expected: clean.

- [ ] **Step 9: Commit**

```bash
git add crates/alexandria-core/src/auth/windows_identity.rs crates/alexandria-core/src/auth/mod.rs crates/alexandria-core/Cargo.toml Cargo.toml Cargo.lock
git commit -m "feat: read the process windows account"
```

---

### Task 3: Wire the mode into startup

The SID gate on both binaries, the `RuntimeAuthService` variant, and the
loopback warning.

**Files:**
- Modify: `crates/alexandria-core/src/auth/mod.rs`
- Modify: `crates/alexandria-core/src/services.rs`
- Modify: `crates/alexandria-http/src/main.rs`
- Modify: `crates/alexandria-ffi/src/lib.rs`

**Interfaces:**
- Consumes: `AuthMode::Windows`, `AuthSettings.windows_owner_sid` (Task 1); `WindowsIdentity`, `ProcessWindowsIdentity`, `verify_owner` (Task 2).
- Produces: `RuntimeAuthService::Windows(local::LocalAuthService<local::SqliteSessionRepository, crate::catalog::clock::SystemClock>)`, whose `mode()` returns `AuthMode::Windows`.

- [ ] **Step 1: Add the runtime variant**

In `crates/alexandria-core/src/auth/mod.rs`, add to `RuntimeAuthService`:

```rust
    /// UC-45. The same session validation `Local` uses, deliberately: the two
    /// modes differ in how a session is *obtained*, never in how one is
    /// checked, and a second implementation would be a second place for the
    /// expiry rule to drift.
    Windows(
        local::LocalAuthService<local::SqliteSessionRepository, crate::catalog::clock::SystemClock>,
    ),
```

and the two match arms — `authenticate` delegating to the inner service, and
`mode` returning `AuthMode::Windows`.

- [ ] **Step 2: Add the services arm**

In `crates/alexandria-core/src/services.rs`, extend the `match settings.auth.mode`:

```rust
        // UC-45: the account this process runs as was verified at startup,
        // before this point. What remains is session validation, which is
        // exactly local mode's — hence the same service behind a different
        // variant.
        AuthMode::Windows => {
            RuntimeAuthService::Windows(LocalAuthService::new(session_repo.clone(), clock))
        }
```

- [ ] **Step 3: Gate the HTTP binary's startup**

In `crates/alexandria-http/src/main.rs`, after the existing
`settings.auth.validate()?;` and before `let bind_addr = …`:

```rust
    // UC-45 / FR-AU-21: in Windows mode the account this process runs as *is*
    // the credential, so a process running as anyone else must not serve the
    // catalog. Checked once, here, because a process cannot change the account
    // it runs as.
    if settings.auth.mode == AuthMode::Windows {
        verify_owner(&ProcessWindowsIdentity, &settings.auth.windows_owner_sid)?;
    }
```

Then, immediately after `let bind_addr = settings.http.socket_addr();`:

```rust
    // FR-AU-24: Windows mode proves the process was launched by the owner, not
    // who is calling it — so any caller that can reach the port is authorized.
    // On loopback that is the owner's own machine. Anywhere else it is the
    // network, and the operator should hear about it.
    if settings.auth.mode == AuthMode::Windows && !bind_addr.ip().is_loopback() {
        tracing::warn!(
            %bind_addr,
            "auth.mode is \"windows\" and the bind address is not loopback: in this mode \
             any caller that can reach the port is authorized"
        );
    }
```

Add `use alexandria_core::auth::windows_identity::{verify_owner, ProcessWindowsIdentity};`
and `use alexandria_core::config::AuthMode;` to the imports.

- [ ] **Step 4: Gate the FFI initializer**

In `crates/alexandria-ffi/src/lib.rs`, inside `alexandria_index_init`, extend
the existing validation block:

```rust
    // Same gate as the HTTP binary: a misconfigured mode is a startup failure
    // on both surfaces (FR-AU-08).
    if settings.auth.validate().is_err() {
        return INDEX_ERR_OTHER;
    }
    if settings.auth.mode == AuthMode::Windows
        && verify_owner(&ProcessWindowsIdentity, &settings.auth.windows_owner_sid).is_err()
    {
        return INDEX_ERR_OTHER;
    }
```

with the matching imports. There is no bind address on this surface, so no
warning belongs here.

- [ ] **Step 5: Confirm everything compiles and the targeted suites pass**

Run:

```bash
cargo build --workspace --all-targets
cargo test -p alexandria-core --test auth
cargo test -p alexandria-core --test config
```

Expected: all pass. The new `AuthMode` variant may make `match` statements
elsewhere non-exhaustive — fix each one explicitly rather than adding a
catch-all arm.

- [ ] **Step 6: Check lints and formatting**

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat: refuse to start as the wrong account"
```

---

### Task 4: The login operation

**Files:**
- Create: `crates/alexandria-core/src/auth/commands/windows_login.rs`
- Modify: `crates/alexandria-core/src/auth/commands/mod.rs`, `crates/alexandria-core/src/services.rs`
- Modify: `crates/alexandria-http/src/routes/auth.rs`, `crates/alexandria-http/src/lib.rs`, `crates/alexandria-ffi/src/lib.rs`
- Create: `crates/alexandria-core/tests/auth/windows_login.rs`, declared in `tests/auth.rs` as `#[path = "auth/windows_login.rs"] mod windows_login;`
- Modify: `crates/alexandria-ffi/tests/parity.rs`

**Interfaces:**
- Consumes: `AuthMode::Windows` (Task 1); `issue_session`, `SessionRepository`, `LocalLoginResult` from `crate::auth::local`; `Clock`.
- Produces:
  - `WindowsLoginHandler::new(sessions, clock, mode, session_ttl_hours)` with `async fn login(&self) -> Result<LocalLoginResult, DomainError>`
  - `POST /v1/auth/windows/login`
  - `alexandria_auth_windows_login(json_body: *const c_char) -> AuthJsonResult`

`LocalLoginResult { success: bool, session_id: Uuid }` is reused rather than a
new type: the two modes return the same thing, and a second identical struct
would be two shapes for one idea.

- [ ] **Step 1: Write the failing tests**

Create `crates/alexandria-core/tests/auth/windows_login.rs`. Model the helpers on
`crates/alexandria-core/tests/auth/login.rs` — read it first; it already fakes a
session repository and a fixed clock.

```rust
use alexandria_core::auth::commands::windows_login::WindowsLoginHandler;
use alexandria_core::auth::local::SessionRepository;
use alexandria_core::catalog::clock::FixedClock;
use alexandria_core::config::AuthMode;
use alexandria_core::errors::DomainError;

use crate::common::FakeSessionRepository;

const TTL_HOURS: u32 = 24;

fn handler(mode: AuthMode) -> WindowsLoginHandler<FakeSessionRepository, FixedClock> {
    WindowsLoginHandler::new(FakeSessionRepository::new(), clock(), mode, TTL_HOURS)
}

/// Reuse `login.rs`'s fixed clock construction verbatim.
fn clock() -> FixedClock {
    use chrono::{TimeZone, Utc};
    FixedClock(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap())
}

#[tokio::test]
async fn given_windows_mode_when_logged_in_then_a_session_is_opened() {
    let handler = handler(AuthMode::Windows);

    let result = handler.login().await.unwrap();

    assert!(result.success);
    assert!(!result.session_id.is_nil());
}

/// The session must be usable afterwards — minting an id that was never
/// stored would pass a shallower assertion.
#[tokio::test]
async fn given_a_session_from_windows_login_when_checked_then_it_is_valid() {
    let sessions = FakeSessionRepository::new();
    let handler =
        WindowsLoginHandler::new(sessions.clone(), clock(), AuthMode::Windows, TTL_HOURS);

    let result = handler.login().await.unwrap();

    assert!(sessions
        .is_valid(result.session_id, clock().0)
        .await
        .unwrap());
}

#[tokio::test]
async fn given_local_mode_when_windows_login_attempted_then_conflict() {
    let err = handler(AuthMode::Local).login().await.unwrap_err();

    assert!(matches!(err, DomainError::Conflict(_)));
}

#[tokio::test]
async fn given_external_mode_when_windows_login_attempted_then_conflict() {
    let err = handler(AuthMode::External).login().await.unwrap_err();

    assert!(matches!(err, DomainError::Conflict(_)));
}
```

If `FakeSessionRepository` is not `Clone`, or exposes its state differently,
follow whatever `tests/auth/login.rs` already does rather than changing the
fake.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p alexandria-core --test auth windows_login`
Expected: FAIL to compile — `WindowsLoginHandler` does not exist.

- [ ] **Step 3: Write the handler**

Create `crates/alexandria-core/src/auth/commands/windows_login.rs`:

```rust
//! UC-45 — Log in with the Windows account (FR-AU-20, FR-AU-22).
//!
//! Takes no credentials. The account this process runs as was checked against
//! the configured SID at startup, so by the time a caller reaches here the
//! only thing left to do is open a session.
//!
//! What that session proves is worth being clear about: that the process was
//! launched by the owner, never who is calling. In this mode the loopback bind
//! is the security boundary, not the credential — see the design spec.

use crate::auth::local::{issue_session, LocalLoginResult, SessionRepository};
use crate::catalog::clock::Clock;
use crate::config::AuthMode;
use crate::errors::DomainError;

/// Generic over the session repository and clock so the decision logic is
/// unit-tested against trait fakes, then wired with the concrete
/// Sqlite/System collaborators at runtime (services.rs).
pub struct WindowsLoginHandler<SR, C> {
    sessions: SR,
    clock: C,
    mode: AuthMode,
    session_ttl_hours: u32,
}

impl<SR, C> WindowsLoginHandler<SR, C>
where
    SR: SessionRepository,
    C: Clock,
{
    pub fn new(sessions: SR, clock: C, mode: AuthMode, session_ttl_hours: u32) -> Self {
        Self {
            sessions,
            clock,
            mode,
            session_ttl_hours,
        }
    }

    /// Open a session for the owner.
    ///
    /// The process's SID is deliberately *not* re-read here. Startup settled
    /// it, and a process cannot change the account it runs as, so a read per
    /// login would spend a syscall re-answering a closed question.
    pub async fn login(&self) -> Result<LocalLoginResult, DomainError> {
        // AF-01: the active auth mode must be Windows (FR-AU-03).
        if self.mode != AuthMode::Windows {
            return Err(DomainError::conflict(
                "the windows account is not the active auth mode",
            ));
        }

        let session_id =
            issue_session(&self.sessions, &self.clock, self.session_ttl_hours).await?;

        Ok(LocalLoginResult {
            success: true,
            session_id,
        })
    }
}
```

Add `pub mod windows_login;` to `crates/alexandria-core/src/auth/commands/mod.rs`,
alphabetically.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p alexandria-core --test auth windows_login`
Expected: PASS, 4 tests.

- [ ] **Step 5: Wire the handler into services**

In `crates/alexandria-core/src/services.rs`, construct a `WindowsLoginHandler`
beside the existing `LocalLoginHandler`, add it to the `Services` struct with a
type alias in the same style as its neighbours, and pass
`settings.auth.session_ttl_hours` and `settings.auth.mode` exactly as the local
one does.

- [ ] **Step 6: Add the HTTP route**

In `crates/alexandria-http/src/routes/auth.rs`, add a handler in the style of the
existing `local_login` — a doc comment naming the use case, no request body, and
a `200` carrying `LocalLoginResult`. Register it in
`crates/alexandria-http/src/lib.rs` at `POST /v1/auth/windows/login`, **outside**
the authenticated `route_layer`, alongside `/v1/auth/local/login` — a caller has
no session yet, which is the entire point of the call.

- [ ] **Step 7: Add the FFI export**

In `crates/alexandria-ffi/src/lib.rs`, add
`alexandria_auth_windows_login(json_body: *const c_char) -> AuthJsonResult`,
copying the shape of `alexandria_auth_local_login`. It takes a `json_body`
parameter it ignores, for signature consistency with its neighbours on this
surface; say so in a comment.

- [ ] **Step 8: Add a parity test**

In `crates/alexandria-ffi/tests/parity.rs`, add a parity assertion following the
file's existing pattern: drive `POST /v1/auth/windows/login` and
`alexandria_auth_windows_login` against equivalent state in Windows mode and
assert both return a session. Session ids are random, so assert the *shape* —
both succeed, both carry a non-nil `sessionId` — rather than equal bodies.

- [ ] **Step 9: Confirm everything compiles and the targeted suites pass**

Run:

```bash
cargo build --workspace --all-targets
cargo test -p alexandria-core --test auth
cargo test -p alexandria-http
cargo test -p alexandria-ffi
```

Expected: all pass.

- [ ] **Step 10: Check lints and formatting**

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check`
Expected: clean.

- [ ] **Step 11: Commit**

```bash
git add -A
git commit -m "feat: log in with the windows account"
```

---

### Task 5: Documentation

**Files:**
- Modify: `docs/requirements/System Requirements Document.md`, `docs/requirements/Use Case Specification Document.md`, `docs/requirements/Operations & Infrastructure Document.md`, `docs/requirements/Technology Stack Document.md`, `README.md`

- [ ] **Step 1: Reword FR-AU-01 and add the new requirements**

In `docs/requirements/System Requirements Document.md`, replace FR-AU-01 with:

> | FR-AU-01 | The system shall read the active authentication mode from startup configuration; exactly one mode (external JWT, local login, or Windows account) shall be active at runtime. |

and append, keeping the table's format:

| ID | Requirement |
| --- | --- |
| FR-AU-20 | The system shall support a third authentication mode in which the operating system account running the server process is the credential; exactly one mode remains active at runtime. |
| FR-AU-21 | In Windows mode, the system shall refuse to start unless it is running on Windows as the account named by the configured owner SID. |
| FR-AU-22 | In Windows mode, a successful login shall create a Session with the same configurable expiry local mode uses, and the caller shall present that session's id on every subsequent request. |
| FR-AU-23 | In Windows mode, the system shall refuse every local-mode credential and recovery operation, since no credential is stored. |
| FR-AU-24 | The system shall warn at startup when Windows mode is active and the HTTP bind address is not a loopback address, because in that mode any caller that can reach the port is authorized. |

Update §7's endpoint table with a row for `POST /v1/auth/windows/login`
(FR-AU-22), and BR-17's wording in the business-rules table to name three modes.

- [ ] **Step 2: Write UC-45**

In `docs/requirements/Use Case Specification Document.md`, add after UC-44,
following the document's exact field and table format:

**UC-45: Log in with the Windows account** — Actors: Owner, Operating System.
Description: Open a session on the strength of the Windows account the server
process runs as. Preconditions: the active auth mode is the Windows account; the
process passed its startup account check. Postconditions: a Session exists whose
id is returned to the caller. Requirements: FR-AU-20, FR-AU-22.

Main flow: (1) the caller requests a Windows login, submitting nothing; (2) the
system confirms the active auth mode is the Windows account; (3) the system
creates a Session with an expiry `sessionTtlHours` in the future and returns its
id.

| ID | Condition | Outcome |
| --- | --- | --- |
| AF-01 | The active auth mode is local login or external JWT | The system rejects with an invalid-operation error. |
| AF-02 | The session cannot be created | The system returns the underlying error; no session exists. |

Add below the table:

> This use case has no unauthorized flow, and that is the point of it rather
> than an omission. The account check happens once at startup: a process running
> as anyone but the configured owner does not reach the point of serving
> requests. What that proves is that the process was launched by the owner —
> never who is calling — so in this mode any caller that can reach the port is
> authorized, and the loopback bind is the security boundary.

Add UC-45 to the §3 traceability table.

- [ ] **Step 3: Update the operations document**

In `docs/requirements/Operations & Infrastructure Document.md`, add
`auth.windows_owner_sid` to the configuration table (source: config /
`ALEXANDRIA_AUTH_WINDOWS_OWNER_SID`; notes: Windows mode only, required, startup
fails unless the process runs on Windows as this account; find it with
`whoami /user`; not a secret). Then add:

```markdown
#### The Windows account mode

`auth.mode = "windows"` makes the Windows account the server process runs as the
credential. Nothing is typed and nothing is stored: the owner's Windows sign-in
is the authentication, and `POST /v1/auth/windows/login` exchanges it for a
session.

Be clear about what this proves before enabling it. It proves the **process was
launched by the configured account** — it does not identify the caller. Once the
process is running, any caller that can reach the port is the owner. The security
boundary in this mode is the loopback bind (IR-03), not the credential, and
startup warns when `http.bind_addr` is anything else.

Startup fails, rather than warns, when the SID is unset, when the platform is not
Windows, or when the process runs as a different account — the last naming both
SIDs, since a mismatch is yours to diagnose and neither value is a secret.

In this mode there is no local account: registration, credential changes, and
recovery-code redemption and regeneration all refuse, exactly as they do in
external mode.
```

- [ ] **Step 4: Update the technology stack document**

Add `windows-sys` to the dependency table, with the justification: reading the
process token needs the Win32 bindings; this is the thinnest available — raw FFI
declarations with no wrapper layer — and it is declared under
`[target.'cfg(windows)'.dependencies]`, so it enters the graph only on Windows
targets and no other platform's build is affected.

- [ ] **Step 5: Update the README**

In `README.md`'s F-09 table, add a row for UC-45 following the existing format,
with an em-dash in the Issue column (no GitHub issue exists) and a checked box:

```
| — | UC-45 | &#9745; | Log in with the Windows account | FR-AU-20, FR-AU-21, FR-AU-22, FR-AU-24 |
```

If the README describes the auth modes in prose anywhere outside that table,
update it to name three.

- [ ] **Step 6: Verify consistency**

Run: `grep -rniE "one mode \(external|either external|external JWT \*\*or\*\*" README.md docs/requirements`
Expected: no match describes only two modes. Fix any that does.

- [ ] **Step 7: Commit**

```bash
git add README.md docs/requirements
git commit -m "docs: describe windows account login"
```

---

## Self-review notes

Checked against the spec:

- "What this mode actually proves" — carried into UC-45's closing note, the Operations passage, and the login handler's doc comment, in the spec's own terms rather than paraphrased.
- Decision 1 (third mode) — Task 1; the five local-mode handlers already gate on `mode != AuthMode::Local`, verified before this plan was written, so FR-AU-23 needs no code.
- Decision 2 (startup, not per request) — Task 3, with the handler's doc comment in Task 4 saying why it does not re-read.
- Decision 3 (login mints a session) — Task 4, with a test that the session is actually usable.
- Decision 4 (reuse local's session validation) — Task 3, step 1: the `Windows` variant wraps `LocalAuthService`.
- Decision 5 (no credential row) — nothing to build; asserted through the refusal tests already covering non-local modes.
- Decision 6 (configured SID, not any account) — Task 1's `validate` arm and Task 2's `verify_owner`.
- Decision 7 (`windows-sys`, target-gated) — Task 2 step 1, documented in Task 5 step 4.
- FR-AU-24's warning — Task 3 step 3.
- The testing table's split between platform-independent and `#[cfg(windows)]` — Tasks 2 and 4.

Deliberately absent, per the spec's out-of-scope section: identifying the calling
process through the TCP table, SSPI negotiation, `LogonUser` password prompting,
and any change to local or external mode beyond FR-AU-01's wording.
