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

        let session_id = issue_session(&self.sessions, &self.clock, self.session_ttl_hours).await?;

        Ok(LocalLoginResult {
            success: true,
            session_id,
        })
    }
}
