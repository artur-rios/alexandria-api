use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::clock::Clock;
use crate::errors::DomainError;
use crate::plays::model::PlayEvent;
use crate::plays::repos::PlayRepository;

/// Record that a track was played (play history design).
///
/// The one write in this module. What counts as "played" — a track heard
/// to the end, or far enough into it — is the player's judgement, not the
/// core's: the core has no idea what the owner is hearing, and a rule
/// invented here would be a second, invisible definition competing with
/// the one the player already applies. The core's contract is narrower and
/// checkable: this track, at this moment, once.
///
/// Carries a `Clock` collaborator, unlike the playlist handlers, because a
/// play is a moment and the moment is the whole record. The caller does not
/// supply it: a client that could name the time could name last year's, and
/// every ranking is an aggregate over that column.
pub struct RecordPlayHandler<A, C, R> {
    auth: A,
    clock: C,
    repo: R,
}

impl<A, C, R> RecordPlayHandler<A, C, R>
where
    A: AuthService,
    C: Clock,
    R: PlayRepository,
{
    pub fn new(auth: A, clock: C, repo: R) -> Self {
        Self { auth, clock, repo }
    }

    /// Record a play of the track identified by `file_uuid`, stamped now.
    ///
    /// `NotFound` when the uuid does not resolve; `InvalidInput` when it
    /// resolves to something that is not audio.
    pub async fn record(&self, file_uuid: Uuid, token: &str) -> Result<PlayEvent, DomainError> {
        // The caller must be authenticated. Evaluated before the payload is
        // consulted (FR-AU-07 / SRD §7).
        self.auth.authenticate(token).await?;

        self.repo.record(file_uuid, self.clock.now()).await
    }
}
