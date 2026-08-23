use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::catalog::run_registry::{RunProgress, RunSignal};
use crate::catalog::runs::{CatalogRunRepository, RunCounts};
use crate::retry::{retry_on_busy, BUSY_ATTEMPTS};

pub mod edit_content;
pub mod edit_metadata;
pub mod index;
pub mod purge;
pub mod purge_on_disk;
pub mod refresh;
pub mod rename;
pub mod restore;
pub mod run_control;
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

/// Record a run that its control signal stopped, best-effort.
///
/// Shared by both walks, which reach it identically: the signal is read once
/// after the in-flight window has drained, and the row is written once, with
/// the tally already flushed. Retried on a busy database like the other
/// terminal writes, and its failure is logged rather than propagated for the
/// reason `finish`'s is — the work the run did actually happened, and the
/// caller's own result must not be replaced by a bookkeeping error. The row
/// stays `running` until startup reconciliation (FR-FC-29) pauses it.
///
/// `counts` is the partial tally the walk reached. Cancel keeps it — a
/// cancelled run is never resumed, so what it got through is final. Pause
/// discards it: a paused run is resumed and re-walks, so a partial tally
/// written now would be superseded, and a resumed run's `already_cataloged`
/// is what describes the re-encountered prefix instead.
///
/// `segment` is the one the calling walk captured before it began, and *both*
/// writes are made conditional on it. By the time this runs the walk's cell is
/// gone, so a pause *and* a resume can both have landed in between — leaving a
/// row that reads `running` because a different segment is walking it, which
/// the status guards alone would accept. Cancel needs it as much as pause and
/// arguably more, being terminal: see [`CatalogRunRepository::cancel`], which
/// also explains why the check does not cost the tally backfill. `None` waives
/// it, leaving only the status guard: it is what a walk with no row to have
/// read a segment from passes.
pub(crate) async fn record_halt<RR>(
    runs: &RR,
    run_id: Uuid,
    signal: RunSignal,
    counts: RunCounts,
    ended_at: DateTime<Utc>,
    segment: Option<i64>,
) where
    RR: CatalogRunRepository,
{
    match signal {
        // Not a halt: both call sites branch on the signal before they get
        // here, and a run nobody stopped has nothing to record either way.
        RunSignal::None => {}
        RunSignal::Pause => {
            match retry_on_busy(BUSY_ATTEMPTS, || runs.pause(run_id, ended_at, segment)).await {
                // The pause was refused. Either the row is no longer
                // `running` — something else closed this run while the walk
                // was between dropping its cell and getting here, in practice
                // a `cancel` that found no live cell and wrote itself
                // directly — or it is `running` again on a later segment,
                // because a pause and a resume both landed in that same gap.
                // Both writes stand, deliberately, and this one is dropped.
                // Logged rather than swallowed in either case: silence here
                // would hide the next bug of this shape, which is how both of
                // these were found.
                Ok(false) => tracing::warn!(
                    %run_id,
                    ?segment,
                    "run was closed or resumed by another caller before it could be paused"
                ),
                Ok(true) => {}
                Err(err) => {
                    tracing::warn!(%run_id, error = %err, "could not record that the run paused")
                }
            }
        }
        RunSignal::Cancel => {
            match retry_on_busy(BUSY_ATTEMPTS, || {
                runs.cancel(run_id, Some(counts), ended_at, segment)
            })
            .await
            {
                // Refused for one of two reasons. Either the row reads
                // `complete` or `failed` — the walk closed itself while this
                // cancel was in flight, and rewriting a run that got through
                // all of its work into a cancelled one is the misreport that
                // guard exists to prevent. Or the row is on a later segment,
                // because a pause and a resume both landed in the gap above,
                // and this cancel is about a run that is already walking
                // again. Either way that write stands and this one is
                // dropped.
                //
                // A cancel that a *control call* already wrote is still not
                // refused: `cancelled` is in this branch's status set and
                // such a call does not move the segment, so the walk lands on
                // top and fills in the four counts the control call had none
                // of. Losing them was avoidable, and a cancel is supposed to
                // keep its tally for the record.
                Ok(false) => tracing::warn!(
                    %run_id,
                    ?segment,
                    "run was closed or resumed by another caller before it could be cancelled"
                ),
                Ok(true) => {}
                Err(err) => {
                    tracing::warn!(%run_id, error = %err, "could not record that the run was cancelled")
                }
            }
        }
    }
}
