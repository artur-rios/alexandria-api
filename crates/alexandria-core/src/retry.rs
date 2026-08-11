//! Bounded retry for transient SQLite write contention.
//!
//! SQLite admits one writer at a time. Under `indexing.concurrency` writers
//! plus a client reading continuously (which also holds off WAL
//! checkpointing), a writer can wait out its whole `busy_timeout` and still be
//! answered `SQLITE_BUSY`. Nothing in the write path retried that, so on a
//! loaded machine a handful of files would simply be counted as `failed` and
//! logged — the exact shape of a CI run that indexed 1998 of 2000 files on a
//! degraded runner.
//!
//! The two pieces here are deliberately separate and both pure: a predicate
//! that says whether an error is that transient condition, and a generic
//! retry loop that consults it. Neither touches sqlx's runtime, so both are
//! unit-tested without a database.

use std::future::Future;
use std::time::Duration;

use crate::errors::DomainError;

/// `SQLITE_BUSY` — the database file is locked by another connection.
///
/// Compared as a string because that is what sqlx's SQLite driver reports:
/// `SqliteError::code` renders the *extended* result code in decimal.
const SQLITE_BUSY: &str = "5";

/// `SQLITE_BUSY_SNAPSHOT` — a WAL-mode deferred transaction cannot upgrade to
/// a write because the snapshot it started from is no longer the newest.
/// Distinct code, same remedy: start over and it will very likely succeed.
const SQLITE_BUSY_SNAPSHOT: &str = "517";

/// How many times a busy write is attempted in total, including the first.
///
/// Three, not more. SQLite has *already* waited out its own `busy_timeout`
/// (5 s, set explicitly in [`crate::migrate`]) before it answers `BUSY`, so
/// every extra attempt is potentially another multi-second stall — the bound
/// is on wall-clock exposure, not on arithmetic. Three attempts turn a
/// momentary pile-up into a success while capping the worst case for one file
/// at roughly three `busy_timeout` waits; a file that is still contended after
/// that is not going to be rescued by a fourth.
pub const BUSY_ATTEMPTS: u32 = 3;

/// Base delay before a retry, doubled each time (50 ms, then 100 ms).
///
/// Small on purpose: the useful wait already happened inside SQLite's busy
/// handler, so this backoff is only there to break the lockstep between
/// concurrent indexer tasks that were all refused at the same instant.
/// Retrying them back-to-back would just reproduce the same collision.
const BUSY_BACKOFF: Duration = Duration::from_millis(50);

/// Whether `error` is SQLite telling us the write lock was unavailable.
///
/// True only for the two busy codes. Everything else — a constraint
/// violation, a pool timeout, a disk error, any non-database `DomainError` —
/// is false, because replaying it would at best waste time and at worst
/// paper over a real bug.
///
/// `SQLITE_LOCKED` (6) is deliberately excluded: it means a *table* in the
/// same connection is locked, which retrying the same statement does not
/// resolve.
pub fn is_retryable_busy(error: &DomainError) -> bool {
    let DomainError::Database(err) = error else {
        return false;
    };
    err.as_database_error()
        .and_then(|db| db.code())
        .is_some_and(|code| code == SQLITE_BUSY || code == SQLITE_BUSY_SNAPSHOT)
}

/// Run `op`, replaying it while it fails with a retryable busy condition.
///
/// `op` is invoked at most `attempts` times and at least once. A success or a
/// non-retryable error returns immediately — the caller sees exactly the
/// `Result` the last invocation produced, so the failure semantics of a write
/// that never succeeds are unchanged; only the odds of reaching one improve.
///
/// Generic over the operation rather than tied to a repository method, which
/// keeps it testable with a plain closure and no database in sight.
pub async fn retry_on_busy<T, F, Fut>(attempts: u32, mut op: F) -> Result<T, DomainError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, DomainError>>,
{
    let attempts = attempts.max(1);
    let mut backoff = BUSY_BACKOFF;
    for attempt in 1..=attempts {
        match op().await {
            Ok(value) => return Ok(value),
            Err(err) if attempt < attempts && is_retryable_busy(&err) => {
                tracing::debug!(
                    attempt,
                    attempts,
                    error = %err,
                    "database is busy; retrying the write"
                );
                tokio::time::sleep(backoff).await;
                backoff *= 2;
            }
            Err(err) => return Err(err),
        }
    }
    // Unreachable: the loop runs at least once and every arm either returns or
    // continues, and the final iteration cannot take the retrying arm.
    Err(DomainError::internal(
        "retry_on_busy exhausted its attempts without a result",
    ))
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::cell::Cell;
    use std::error::Error as StdError;
    use std::fmt;

    use sqlx::error::{DatabaseError, ErrorKind};

    use super::*;
    use crate::errors::DomainError;

    /// A stand-in for `sqlx_sqlite::SqliteError`, which is not constructible
    /// from outside the driver: it is only ever built from a live connection
    /// handle. The one property the predicate reads is `code()`, and the
    /// SQLite driver renders the *extended* result code there as a decimal
    /// string — so reproducing that is enough to exercise the predicate
    /// faithfully without a database.
    #[derive(Debug)]
    struct FakeDatabaseError {
        code: &'static str,
    }

    impl fmt::Display for FakeDatabaseError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "(code: {}) fake database error", self.code)
        }
    }

    impl StdError for FakeDatabaseError {}

    impl DatabaseError for FakeDatabaseError {
        fn message(&self) -> &str {
            "fake database error"
        }

        fn code(&self) -> Option<Cow<'_, str>> {
            Some(Cow::Borrowed(self.code))
        }

        fn as_error(&self) -> &(dyn StdError + Send + Sync + 'static) {
            self
        }

        fn as_error_mut(&mut self) -> &mut (dyn StdError + Send + Sync + 'static) {
            self
        }

        fn into_error(self: Box<Self>) -> Box<dyn StdError + Send + Sync + 'static> {
            self
        }

        fn kind(&self) -> ErrorKind {
            ErrorKind::Other
        }
    }

    fn database_error(code: &'static str) -> DomainError {
        DomainError::Database(sqlx::Error::Database(Box::new(FakeDatabaseError { code })))
    }

    #[test]
    fn given_a_sqlite_busy_error_when_classified_then_it_is_retryable() {
        // Arrange
        let busy = database_error("5");
        let busy_snapshot = database_error("517");

        // Act
        let busy_is_retryable = is_retryable_busy(&busy);
        let busy_snapshot_is_retryable = is_retryable_busy(&busy_snapshot);

        // Assert
        assert!(busy_is_retryable, "SQLITE_BUSY (5) is retryable");
        assert!(
            busy_snapshot_is_retryable,
            "SQLITE_BUSY_SNAPSHOT (517) is retryable"
        );
    }

    #[test]
    fn given_a_non_busy_database_error_when_classified_then_it_is_not_retryable() {
        // Arrange
        // 2067 is SQLITE_CONSTRAINT_UNIQUE — a genuine constraint violation
        // that will fail identically however many times it is replayed.
        let constraint = database_error("2067");
        let locked = database_error("6");

        // Act
        let constraint_is_retryable = is_retryable_busy(&constraint);
        let locked_is_retryable = is_retryable_busy(&locked);

        // Assert
        assert!(
            !constraint_is_retryable,
            "a unique-constraint violation must never be retried"
        );
        assert!(
            !locked_is_retryable,
            "SQLITE_LOCKED (6) is a different condition and is not retried"
        );
    }

    #[test]
    fn given_a_non_database_error_when_classified_then_it_is_not_retryable() {
        // Arrange
        let disk = DomainError::disk("cannot read bytes");
        let not_found = DomainError::NotFound;
        // A `sqlx::Error` that carries no `DatabaseError` at all.
        let pool_timeout = DomainError::Database(sqlx::Error::PoolTimedOut);

        // Act & Assert
        assert!(!is_retryable_busy(&disk));
        assert!(!is_retryable_busy(&not_found));
        assert!(!is_retryable_busy(&pool_timeout));
    }

    #[tokio::test]
    async fn given_an_operation_failing_twice_when_retried_then_it_succeeds_on_the_third_call() {
        // Arrange
        let calls = Cell::new(0usize);

        // Act
        let result = retry_on_busy(3, || async {
            calls.set(calls.get() + 1);
            if calls.get() < 3 {
                Err(database_error("5"))
            } else {
                Ok("written")
            }
        })
        .await;

        // Assert
        assert!(matches!(result, Ok("written")), "the third call succeeded");
        assert_eq!(calls.get(), 3, "exactly three invocations");
    }

    #[tokio::test]
    async fn given_an_operation_always_busy_when_retried_then_it_fails_after_the_bound() {
        // Arrange
        let calls = Cell::new(0usize);

        // Act
        let result: Result<(), DomainError> = retry_on_busy(3, || async {
            calls.set(calls.get() + 1);
            Err(database_error("5"))
        })
        .await;

        // Assert
        assert!(result.is_err(), "the busy error is surfaced unchanged");
        assert_eq!(calls.get(), 3, "the bound is honoured exactly");
    }

    #[tokio::test]
    async fn given_a_non_retryable_error_when_retried_then_it_returns_after_one_call() {
        // Arrange
        let calls = Cell::new(0usize);

        // Act
        let result: Result<(), DomainError> = retry_on_busy(3, || async {
            calls.set(calls.get() + 1);
            // A unique-constraint violation: replaying it can only fail again.
            Err(database_error("2067"))
        })
        .await;

        // Assert
        assert!(result.is_err());
        assert_eq!(
            calls.get(),
            1,
            "a non-retryable error is returned immediately, not replayed"
        );
    }

    #[tokio::test]
    async fn given_an_operation_succeeding_first_when_retried_then_it_runs_once() {
        // Arrange
        let calls = Cell::new(0usize);

        // Act
        let result = retry_on_busy(3, || async {
            calls.set(calls.get() + 1);
            Ok::<_, DomainError>(7)
        })
        .await;

        // Assert
        assert_eq!(result.expect("success"), 7);
        assert_eq!(calls.get(), 1, "the happy path costs no extra work");
    }
}
