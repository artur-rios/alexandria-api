use crate::auth::AuthService;
use crate::errors::DomainError;
use crate::plays::model::MusicStats;
use crate::plays::repos::PlayRepository;

/// How many rows a ranking answers with when the caller does not say.
///
/// Ten: enough to be a chart rather than a podium, short enough to read
/// without scrolling on the smallest supported window.
pub const DEFAULT_LIMIT: i64 = 10;

/// The most any ranking will answer with.
///
/// A cap rather than an unbounded read, because `limit` reaches this
/// straight off a URL query string. A hundred artists is already past what
/// a ranking says anything with; a hundred thousand is a client asking the
/// core to page through the play history one screen could never draw.
pub const MAX_LIMIT: i64 = 100;

/// Read what was played most (play history design).
///
/// Generic over the auth service and the repository, so the decision logic
/// is unit-tested against a trait fake and wired with the concrete
/// Bearer/Sqlite collaborators at runtime (services.rs). Both the HTTP and
/// FFI surfaces call this handler so the two stay at parity (FR-FC-24 /
/// NFR-09).
pub struct MusicStatsHandler<A, R> {
    auth: A,
    repo: R,
}

impl<A, R> MusicStatsHandler<A, R>
where
    A: AuthService,
    R: PlayRepository,
{
    pub fn new(auth: A, repo: R) -> Self {
        Self { auth, repo }
    }

    /// The summary and the four rankings, each cut to `limit` rows
    /// (`DEFAULT_LIMIT` when absent).
    ///
    /// `InvalidInput` when `limit` is below one or above `MAX_LIMIT` —
    /// refused rather than clamped, because a caller that asked for a
    /// thousand and silently got a hundred would report the top hundred as
    /// though it were the whole answer.
    pub async fn read(&self, limit: Option<i64>, token: &str) -> Result<MusicStats, DomainError> {
        // Auth is checked before the payload is consulted (FR-AU-07 /
        // SRD §7).
        self.auth.authenticate(token).await?;

        let limit = limit.unwrap_or(DEFAULT_LIMIT);
        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err(DomainError::InvalidInput(format!(
                "limit must be between 1 and {MAX_LIMIT}"
            )));
        }

        self.repo.stats(limit).await
    }
}
