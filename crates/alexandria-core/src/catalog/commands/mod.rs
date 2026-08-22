use uuid::Uuid;

use crate::catalog::run_registry::RunProgress;
use crate::catalog::runs::CatalogRunRepository;

pub mod edit_content;
pub mod edit_metadata;
pub mod index;
pub mod purge;
pub mod purge_on_disk;
pub mod refresh;
pub mod rename;
pub mod restore;
pub mod soft_delete;

/// How often an in-flight run flushes its progress into its record
/// (FR-FC-28), in seconds.
///
/// The flush is not what a client watching a live run reads — that comes from
/// the in-memory cell, which is exact. It is only what a *stopped* run leaves
/// behind, so the interval trades one write every two seconds against how
/// much of a stopped run's tally is lost. Two seconds costs a large run a few
/// hundred writes over an hour, which is nothing beside the per-file writes
/// the same run is already making, and bounds the loss to the work of two
/// seconds.
pub(crate) const PROGRESS_FLUSH_SECONDS: i64 = 2;

/// Flush one run's progress into its record, best-effort.
///
/// A failure is logged at `warn` and swallowed: the in-memory cell is
/// authoritative while the run executes, so a missed flush costs accuracy
/// after a restart, not correctness, and failing a run over a bookkeeping
/// write would throw away real work. It is not wrapped in `retry_on_busy`
/// either — the next flush is two seconds away and writes the same fact, so
/// retrying here would only hold up the processing loop to deliver a number
/// that is about to be superseded.
pub(crate) async fn flush_progress<RR>(runs: &RR, run_id: Uuid, progress: &RunProgress)
where
    RR: CatalogRunRepository,
{
    if let Err(err) = runs.record_progress(run_id, progress).await {
        tracing::warn!(%run_id, error = %err, "could not flush run progress");
    }
}
