//! Live progress for in-flight runs (UC-42 / FR-FC-28).
//!
//! A run in flight used to have status `running` and nothing else: counts were
//! written once, at the end, by `runs.finish`. A client could not draw a
//! progress bar because the core published no number to draw one from.
//!
//! Progress lives here, in a per-run cell of atomics, rather than in the
//! database: the processing loop touches it once per file, and a row update
//! per file would put a SQLite write in front of every entry — the exact cost
//! FR-FC-08 keeps off the indexing path. The cell is flushed into
//! `catalog_runs` periodically instead (see the handlers), which is what lets
//! a run that stopped still report its last known tally.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use uuid::Uuid;

/// Which half of a run is executing (FR-FC-28).
///
/// `Discovering` is enumeration — a filesystem walk for an index, a
/// `list_all` for a refresh — during which the denominator is not yet known.
/// `Processing` is the per-entry loop, where both numbers are meaningful.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunPhase {
    Discovering,
    Processing,
}

impl RunPhase {
    /// The stored form. Kept as text, like `kind` and `status`, so a row is
    /// readable without a lookup table.
    pub fn as_str(self) -> &'static str {
        match self {
            RunPhase::Discovering => "discovering",
            RunPhase::Processing => "processing",
        }
    }

    /// `None` for an unrecognized value. The caller decides whether that is
    /// an error; for a display-only field, "unknown" is not worth failing a
    /// read over.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "discovering" => Some(RunPhase::Discovering),
            "processing" => Some(RunPhase::Processing),
            _ => None,
        }
    }

    /// The `AtomicU8` encoding. Private to this module — the discriminants
    /// are an implementation detail of [`RunCell`], not a stored format.
    fn as_code(self) -> u8 {
        match self {
            RunPhase::Discovering => 0,
            RunPhase::Processing => 1,
        }
    }

    fn from_code(code: u8) -> Self {
        match code {
            1 => RunPhase::Processing,
            // A cell is born `Discovering` and only ever set to a value
            // `as_code` produced, so this arm is unreachable in practice.
            // Defaulting beats panicking in a display path.
            _ => RunPhase::Discovering,
        }
    }
}

/// One read of a [`RunCell`]. `total` is `None` while discovery is still
/// counting — see [`TOTAL_UNKNOWN`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunProgress {
    pub phase: RunPhase,
    pub total: Option<usize>,
    pub processed: usize,
}

/// The sentinel [`RunCell::total`] holds until discovery reports a real count.
///
/// A sentinel rather than an `Option` because the field has to be an atomic —
/// there is no `AtomicOption<usize>`, and wrapping the whole cell in a mutex
/// to make one nullable field expressible would put a lock on the per-file
/// path. `usize::MAX` is safe as the "unknown" marker: it means a library of
/// exactly `usize::MAX` files would report an unknown total, which is not a
/// reachable library.
const TOTAL_UNKNOWN: usize = usize::MAX;

/// The live progress of one run: a phase, a denominator, and a counter.
///
/// Every field is `Ordering::Relaxed`. These counters are read for display,
/// and a reader that sees `processed` one increment behind — or sees the new
/// `total` a moment before the phase that goes with it — has read a progress
/// bar that is momentarily stale, not a wrong answer. Nothing branches on
/// them, and no other state is published through them, so there is nothing
/// for an `Acquire`/`Release` pair to order. Paying for a fence on every
/// indexed file to make a progress bar a few microseconds fresher is not a
/// trade worth making.
#[derive(Debug)]
pub struct RunCell {
    processed: AtomicUsize,
    total: AtomicUsize,
    phase: AtomicU8,
}

impl RunCell {
    fn new() -> Self {
        Self {
            processed: AtomicUsize::new(0),
            total: AtomicUsize::new(TOTAL_UNKNOWN),
            phase: AtomicU8::new(RunPhase::Discovering.as_code()),
        }
    }

    pub fn set_phase(&self, phase: RunPhase) {
        self.phase.store(phase.as_code(), Ordering::Relaxed);
    }

    /// Publish the denominator once discovery has counted it.
    pub fn set_total(&self, total: usize) {
        self.total.store(total, Ordering::Relaxed);
    }

    /// One more entry finished. Called once per entry regardless of that
    /// entry's outcome — an entry that was skipped or failed is still an
    /// entry the run is done with, and a progress bar that stalled on the
    /// unreadable files would misreport how far along the run is.
    pub fn advance(&self) {
        self.processed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> RunProgress {
        let total = self.total.load(Ordering::Relaxed);
        RunProgress {
            phase: RunPhase::from_code(self.phase.load(Ordering::Relaxed)),
            total: (total != TOTAL_UNKNOWN).then_some(total),
            processed: self.processed.load(Ordering::Relaxed),
        }
    }
}

/// The live cells of every run currently executing in this process.
///
/// Cloning shares the same map: the indexing handlers write cells into it and
/// the status query reads them out, and they are separate handlers holding
/// separate clones of one registry.
///
/// The mutex is taken only to open, find, or close a run — three times per
/// run plus once per status query — never on the per-file path, which touches
/// the `Arc<RunCell>` it already holds.
#[derive(Debug, Clone, Default)]
pub struct RunRegistry {
    cells: Arc<Mutex<HashMap<Uuid, Arc<RunCell>>>>,
}

impl RunRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `run_id` as live and hand back its cell. Re-opening an id
    /// replaces its cell, so a run always starts from zero.
    pub fn open(&self, run_id: Uuid) -> Arc<RunCell> {
        let cell = Arc::new(RunCell::new());
        self.lock().insert(run_id, Arc::clone(&cell));
        cell
    }

    /// The live cell for `run_id`, or `None` when no run by that id is
    /// executing here — which is the signal to fall back to the persisted
    /// row.
    pub fn get(&self, run_id: Uuid) -> Option<Arc<RunCell>> {
        self.lock().get(&run_id).map(Arc::clone)
    }

    /// Drop `run_id`'s cell. Called when a run terminates, however it
    /// terminates, so the map does not grow with the process's uptime.
    pub fn close(&self, run_id: Uuid) {
        self.lock().remove(&run_id);
    }

    /// A poisoned lock is recovered from rather than propagated: the map is a
    /// plain `HashMap` with no invariant a panic mid-insert could have left
    /// broken, and refusing every future progress read because one unrelated
    /// thread panicked would turn a display concern into an outage.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, Arc<RunCell>>> {
        self.cells.lock().unwrap_or_else(|err| err.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_a_fresh_cell_when_snapshotted_then_it_is_discovering_with_no_total() {
        let cell = RunRegistry::new().open(Uuid::new_v4());

        let progress = cell.snapshot();

        assert_eq!(progress.phase, RunPhase::Discovering);
        assert_eq!(progress.total, None, "discovery has not counted yet");
        assert_eq!(progress.processed, 0);
    }

    #[test]
    fn given_a_counted_run_when_advanced_then_the_snapshot_reports_both_numbers() {
        let cell = RunRegistry::new().open(Uuid::new_v4());
        cell.set_phase(RunPhase::Processing);
        cell.set_total(12_264);
        for _ in 0..8_412 {
            cell.advance();
        }

        let progress = cell.snapshot();

        assert_eq!(progress.phase, RunPhase::Processing);
        assert_eq!(progress.total, Some(12_264));
        assert_eq!(progress.processed, 8_412);
    }

    #[test]
    fn given_an_open_run_when_fetched_then_the_same_cell_is_returned() {
        let registry = RunRegistry::new();
        let run_id = Uuid::new_v4();
        let cell = registry.open(run_id);
        cell.advance();

        let fetched = registry.get(run_id).expect("an open run has a cell");

        assert_eq!(fetched.snapshot().processed, 1);
        assert!(
            Arc::ptr_eq(&cell, &fetched),
            "get must hand out the writer's own cell, not a copy"
        );
    }

    #[test]
    fn given_a_closed_run_when_fetched_then_there_is_no_cell() {
        let registry = RunRegistry::new();
        let run_id = Uuid::new_v4();
        registry.open(run_id);

        registry.close(run_id);

        assert!(registry.get(run_id).is_none());
    }

    #[test]
    fn given_two_runs_when_one_advances_then_the_other_is_untouched() {
        let registry = RunRegistry::new();
        let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
        let first_cell = registry.open(first);
        registry.open(second);

        first_cell.advance();

        assert_eq!(registry.get(first).unwrap().snapshot().processed, 1);
        assert_eq!(registry.get(second).unwrap().snapshot().processed, 0);
    }

    #[test]
    fn given_a_registry_clone_when_a_run_opens_then_the_clone_sees_it() {
        // The handlers hold separate clones of one registry: the indexer
        // writes cells and the status query reads them.
        let writer = RunRegistry::new();
        let reader = writer.clone();
        let run_id = Uuid::new_v4();

        writer.open(run_id).advance();

        assert_eq!(reader.get(run_id).unwrap().snapshot().processed, 1);
    }

    #[test]
    fn given_a_phase_when_round_tripped_through_its_stored_form_then_it_is_unchanged() {
        for phase in [RunPhase::Discovering, RunPhase::Processing] {
            assert_eq!(RunPhase::parse(phase.as_str()), Some(phase));
        }
        assert_eq!(RunPhase::parse("nonsense"), None);
    }
}
