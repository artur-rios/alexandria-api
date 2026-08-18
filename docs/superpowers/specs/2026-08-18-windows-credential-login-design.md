# Design: Windows credential login (F-09 — UC-45)

**Date:** 2026-08-18
**Status:** Approved, ready for implementation planning
**Tracks:** Extends an existing capability area — Pluggable Authentication (AU).
Milestone F-09.

## Context

Alexandria has two authentication modes. External mode verifies a Heimdall-issued
JWT against a shared HS256 secret. Local mode verifies an e-mail and password
against an Argon2 hash and, since the recovery-codes change, hands the owner ten
codes as their only way back in.

Both ask the owner to hold a secret. On the machine this software is built for —
a single-user Windows desktop, with the Flutter front-end running in-process over
FFI — the owner has already proved who they are: they signed in to Windows. A
third credential is a third thing to lose.

This design adds a third mode in which the operating system is the authenticator.
Nothing is typed, nothing is stored, and nothing can be forgotten.

Alexandria stays single-owner. Nothing here relaxes BR-01.

## What this mode actually proves

Stated first, and plainly, because everything else follows from it and because a
reader who skims must not miss it.

The credential is **the Windows account the server process is running as**. That
proves *the process was launched by the owner*. It does **not** identify the
caller: a loopback TCP connection carries no identity, and this design
deliberately does not go looking for one.

So once the process is running, **anyone who can reach the port is the owner.**
The security boundary in Windows mode is the loopback bind (IR-03), not the
credential.

That is an honest trade for a desktop application whose front-end is the same
process, and a bad one for anything reachable off the machine. Two consequences
are therefore part of the design rather than footnotes:

- Startup **logs a warning** when Windows mode is active and `http.bind_addr` is
  not a loopback address. An operator who moves the bind address in this mode has
  handed their catalog to the network, and the process should say so.
- The mode's documentation says the above in the same words, in the Operations
  document, where an operator will meet it.

## Decisions

1. **A third mode, not a second method inside local mode.**
   `auth.mode = "windows"`, mutually exclusive with `local` and `external`.
   FR-AU-01 and BR-17 keep their meaning — exactly one mode active at runtime —
   and gain a third value.

   The alternative, letting local mode accept either a password or the Windows
   identity, was rejected because the two are not equivalent. Local mode is built
   around a stored credential and ten recovery codes; a second path that any
   caller reaching the port satisfies would silently bypass both, and every
   local-mode operation would need to say what it does when the caller
   authenticated the other way. Keeping the modes separate keeps each one's
   security model stateable in a sentence.

2. **The check runs at startup, not per request.**
   Reading the process token answers the same question on every request, because
   a process does not change the account it runs as. Checking once at startup and
   refusing to run on a mismatch turns a per-request cost into a configuration
   error, which is where it belongs — and matches how external mode already
   refuses to start without its signing secret.

3. **Login still mints a session.**
   `POST /v1/auth/windows/login` takes no body and returns a session id, which
   every later request carries as a bearer token. The session adds no security
   here — a caller who can reach login can reach everything — and it is kept
   anyway for three reasons: one client contract across all three modes, so the
   front-end's auth code does not branch on mode; the HTTP middleware and the
   `AuthService` trait stay untouched; and if this mode is ever tightened to
   identify the caller properly, no client changes.

4. **The session half is reused, not reimplemented.**
   `RuntimeAuthService` gains a `Windows` variant that validates session ids
   through the same repository and clock as `Local`. Two mode variants that
   validate sessions differently would be two places for the expiry rule to
   drift.

5. **No credential row, no password, no recovery codes.**
   The OS is the authenticator, so there is nothing to store and nothing to
   recover. Register, set-credentials, redeem, regenerate and account all refuse
   in Windows mode with the invalid-operation rejection they already return in
   external mode (FR-AU-03). That refusal path exists and is tested; this adds a
   third value to it rather than a new mechanism.

6. **A configured SID, not "whoever happens to run it".**
   `auth.windows_owner_sid` names the account. Without it the mode would be "no
   authentication at all", and a process started as SYSTEM by a misconfigured
   service manager, or as the wrong account entirely, would serve the catalog
   anyway. A SID rather than a username because usernames are renameable and
   reusable while a SID is not.

7. **`windows-sys`, gated to Windows targets.**
   Reading a process token needs the Win32 bindings. `windows-sys` is the
   thinnest one available — raw FFI declarations with no wrapper layer — and it
   is declared under `[target.'cfg(windows)'.dependencies]` so it never enters
   the dependency graph on Linux or macOS.

   The Technology Stack Document's dependency discipline is the reason this is a
   decision rather than an incidental. The alternative considered was shelling
   out to `whoami /user` and parsing its output, which trades a declared
   dependency for an undeclared one on a console program's output format.

## Architecture

The `AuthService` trait boundary does not move. No handler, no middleware, and no
FFI export outside the new operation changes.

### New

**`auth/windows_identity.rs`** — one port and two implementations:

```rust
pub trait WindowsIdentity: Send + Sync {
    /// The SID of the account this process runs as, as a string
    /// (`S-1-5-21-…`).
    fn current_sid(&self) -> Result<String, DomainError>;
}
```

- `ProcessWindowsIdentity`, behind `#[cfg(windows)]`: opens the process token,
  reads its `TokenUser`, and converts the SID with `ConvertSidToStringSidW`,
  freeing the buffer afterwards. The whole `windows-sys` surface of this feature
  lives in this one function.
- `UnsupportedWindowsIdentity`, behind `#[cfg(not(windows))]`: always returns a
  configuration error naming the platform, so starting with `mode = "windows"` on
  Linux is a clear startup failure rather than a mysterious authentication one.

The trait exists so every decision around it is testable on every platform
against a fake — which is most of the feature.

**`auth/commands/windows_login.rs`** — checks the mode, then mints a session
through the same helper `LocalLoginHandler` uses.

**`WindowsAuthService`** in `auth/mod.rs`'s `RuntimeAuthService` — a `Windows`
variant wrapping the same session repository and clock the `Local` variant uses.

### Changed

- `config.rs`: `AuthMode` gains `Windows`; `AuthSettings` gains
  `windows_owner_sid: String` with an `ALEXANDRIA_AUTH_WINDOWS_OWNER_SID`
  override; `AuthSettings::validate` gains the Windows-mode arm.
- `services.rs`: a third arm on the `AuthMode` match.
- The five local-mode handlers already reject a non-local mode; their checks read
  `mode != AuthMode::Local` and so need no change, which is worth verifying
  rather than assuming during implementation.

### Configuration

| Key | Required | Purpose |
| --- | --- | --- |
| `auth.windows_owner_sid` | in Windows mode | The SID of the account the process must run as, e.g. `S-1-5-21-1004336348-1177238915-682003330-1001`. |

Startup fails when Windows mode is active and the key is empty, when the platform
is not Windows, or when the process's actual SID does not equal the configured
one. The third message names both SIDs: a mismatch is a configuration error the
operator has to be able to diagnose, and neither value is a secret — a SID is an
identifier, not a credential.

### Operations

| Operation | HTTP | FFI | Auth | Effect |
| --- | --- | --- | --- | --- |
| Windows login | `POST /v1/auth/windows/login` | `alexandria_auth_windows_login` | none | Returns a session id |

One operation, on both surfaces (FR-AU-08).

### Data flow

1. **Startup:** `AuthSettings::validate` confirms the key is set; `services.rs`
   then reads the process SID and compares. A failure at either point stops the
   process before it binds a port.
2. **Login:** the handler confirms the active mode is Windows and mints a session
   with the configured TTL. It does *not* re-read the SID — startup settled that,
   and a per-request read would answer the same question at a syscall's cost.
3. **Every other request:** the middleware validates the session id exactly as it
   does in local mode.

### Error handling

| Condition | Outcome |
| --- | --- |
| `auth.windows_owner_sid` empty in Windows mode | Startup fails, naming the key |
| Not running on Windows, in Windows mode | Startup fails, naming the platform |
| Process SID ≠ configured SID | Startup fails, naming both |
| Windows mode active, `http.bind_addr` not loopback | Startup **warns** and continues — it is a deployment choice, not an error |
| Login called in another mode | Invalid-operation rejection (FR-AU-03) |
| Unknown or expired session id | `401`, as in local mode |
| A local-mode operation called in Windows mode | Invalid-operation rejection (FR-AU-03) |

No new machine-readable reason codes: Windows login takes no input, so there is
no input to reject.

## Requirements

New, joining FR-AU-01 … FR-AU-19:

| ID | Requirement |
| --- | --- |
| FR-AU-20 | The system shall support a third authentication mode in which the operating system account running the server process is the credential; exactly one mode remains active at runtime. |
| FR-AU-21 | In Windows mode, the system shall refuse to start unless it is running on Windows as the account named by the configured owner SID. |
| FR-AU-22 | In Windows mode, a successful login shall create a Session with the same configurable expiry local mode uses, and the caller shall present that session's id on every subsequent request. |
| FR-AU-23 | In Windows mode, the system shall refuse every local-mode credential and recovery operation, since no credential is stored. |
| FR-AU-24 | The system shall warn at startup when Windows mode is active and the HTTP bind address is not a loopback address, because in that mode any caller that can reach the port is authorized. |

FR-AU-01 and BR-17 are reworded to say "exactly one mode (external JWT, local
login, or Windows account)" rather than naming two.

**UC-45: Log in with the Windows account** joins the Use Case Specification
Document with its alternative flows and its traceability row.

## Testing

Per the [Testing Specification Document](../../requirements/Testing%20Specification%20Document.md)
§6, and split by what is genuinely platform-dependent.

**Runs on every platform, including Linux CI**, because the decision logic is
generic over `WindowsIdentity` and tested against a hand-written fake:

| Case | Expected |
| --- | --- |
| Login in Windows mode | returns a session id, valid for the configured TTL |
| Login in local mode | invalid-operation rejection |
| Login in external mode | invalid-operation rejection |
| A session from Windows login, presented later | authenticates |
| An unknown or expired session id | unauthorized |
| Startup: SID matches | proceeds |
| Startup: SID differs | fails, message names both SIDs |
| Startup: SID unreadable (the non-Windows stub) | fails, message names the platform |
| Config: Windows mode with an empty `windows_owner_sid` | `validate` fails, naming the key |
| Config: `mode = "windows"` parses from TOML and from the env override | round-trips |
| Register, set-credentials, redeem, regenerate, account, in Windows mode | each refuses |
| Bind address not loopback in Windows mode | warns, does not fail |

**Windows only**, one test behind `#[cfg(windows)]`: `ProcessWindowsIdentity`
returns a well-formed SID string beginning `S-1-5-` and containing no interior
NUL. That is the entire untestable-elsewhere surface, and it asserts shape rather
than a value, since the value differs per machine.

Plus HTTP/FFI parity for the login operation, per §7.3.

## Out of scope

- **Identifying the calling process** — resolving a loopback peer's PID through
  the Windows TCP table and opening its token. It would make the mode prove what
  its name suggests, and it is a substantial amount of Windows API work whose
  PID-to-connection lookup is racy by nature. If this mode ever needs to be
  stronger, this is the change to make, and decision 3 keeps clients unaffected
  by it.
- **SSPI / Kerberos negotiation**, which is the right answer for a service on a
  real network and the wrong shape for a loopback desktop API.
- **Prompting for a Windows password** via `LogonUser`. It would put a Windows
  account password — a far more valuable secret than the one it replaces —
  through this process in plaintext, to prove something the sign-in already
  proved.
- Any change to local or external mode beyond the reworded FR-AU-01.
