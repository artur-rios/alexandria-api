use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::clock::Clock;
use crate::catalog::run_registry::{RunRegistry, RunSignal};
use crate::catalog::runs::{CatalogRunRepository, RunKind, RunPriority, RunStatus};
use crate::errors::DomainError;
use crate::retry::{retry_on_busy, BUSY_ATTEMPTS};

/// Pause or cancel an index or re-index run in flight (UC-42).
///
/// A run answers immediately with an id and then walks a library for minutes
/// (FR-FC-08). Until now the only way to stop one was to stop the process.
/// This is the other end of the run registry Task 6 built: the walk publishes
/// its progress into a cell, and this handler writes a signal back into the
/// same cell, which the walk reads before every entry.
///
/// Why a signal rather than aborting the task: the walk owns a tally and a
/// row, and dropping it mid-flight would leave both half-written. Letting the
/// in-flight window drain costs milliseconds — per-file work is a stat and a
/// header read, not a full-file hash (FR-FC-09/FR-FC-10) — and buys a run
/// that records its own stopping point exactly once, in the same place it
/// would have recorded its completion.
///
/// Generic over the auth service, the run repository, and the clock, then
/// wired with the concrete Runtime/Sqlite/System collaborators at runtime
/// (services.rs). The clock is needed for the one case that writes the row
/// from here — a run with no live cell, below. The registry is concrete: a
/// process-local map of atomics with no I/O behind it, so there is nothing to
/// fake.
pub struct RunControlHandler<A, RR, C> {
    auth: A,
    runs: RR,
    clock: C,
    registry: RunRegistry,
    /// The width a `RunPriority::Normal` run is walked at
    /// (`indexing.concurrency`), and the width to resume a run at when its own
    /// row records none. A run started before the priority column was ever
    /// written has no stored width, and resuming it at the configured default
    /// is the only answer available that is not invented.
    default_concurrency: u32,
    /// The width a `RunPriority::Low` run is walked at
    /// (`indexing.low_priority_concurrency`). Held here for the same reason
    /// `IndexHandler` holds it: resume may now be handed a priority, and
    /// resolving one into a width is a question only the configuration can
    /// answer.
    low_priority_concurrency: u32,
}

/// What a caller needs to put a resumed run back to work (UC-42).
///
/// `resume` records the state change and hands this back rather than spawning
/// anything, mirroring how `start` and `execute` are already separated: the
/// FFI and HTTP layers own the runtime, and a handler that spawned would take
/// that decision away from them and force `Send + 'static` onto collaborators
/// that have no other reason to carry it.
///
/// `kind` is what says which handler's `execute` to spawn, and `root` is
/// `None` for a refresh, which takes none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResumed {
    pub run_id: Uuid,
    pub root: Option<String>,
    pub kind: RunKind,
    pub concurrency: u32,
}

/// Which verb was asked for. Private, and deliberately not [`RunSignal`]:
/// `RunSignal::None` is not a request a caller can make, and modelling it
/// here would force an unreachable arm into every match below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verb {
    Pause,
    Cancel,
}

impl Verb {
    fn signal(self) -> RunSignal {
        match self {
            Verb::Pause => RunSignal::Pause,
            Verb::Cancel => RunSignal::Cancel,
        }
    }

    /// Whether a run in `status` may be asked to do this.
    ///
    /// Only a `Running` run can be paused — pausing a paused run is a
    /// no-op the caller should be told about rather than silently accepted,
    /// and the three terminal statuses have no run left to stop.
    ///
    /// Cancel additionally accepts a `Paused` run: abandoning one is the
    /// whole reason to cancel rather than pause, and without it a paused run
    /// could never be got rid of.
    fn permits(self, status: RunStatus) -> bool {
        match self {
            Verb::Pause => status == RunStatus::Running,
            Verb::Cancel => matches!(status, RunStatus::Running | RunStatus::Paused),
        }
    }
}

impl<A, RR, C> RunControlHandler<A, RR, C>
where
    A: AuthService,
    RR: CatalogRunRepository,
    C: Clock,
{
    /// `default_concurrency` is `indexing.concurrency` and
    /// `low_priority_concurrency` is `indexing.low_priority_concurrency` —
    /// the same two numbers `IndexHandler::new` takes, and clamped the same
    /// way, because a run resumed at a priority must land on exactly the
    /// width a run *started* at that priority would have (FR-FC-08). Zero is
    /// meaningless for either: a walk that processes no files at a time is
    /// not a slower walk, it is no walk.
    pub fn new(
        auth: A,
        runs: RR,
        clock: C,
        registry: RunRegistry,
        default_concurrency: u32,
        low_priority_concurrency: u32,
    ) -> Self {
        Self {
            auth,
            runs,
            clock,
            registry,
            default_concurrency: default_concurrency.max(1),
            low_priority_concurrency: low_priority_concurrency.max(1),
        }
    }

    /// The width a run at `priority` should be walked at — the same mapping
    /// `IndexHandler::concurrency_for` makes, and deliberately duplicated
    /// rather than shared: this handler must not depend on `IndexHandler`,
    /// which it would otherwise have to for the refresh case too.
    fn concurrency_for(&self, priority: RunPriority) -> u32 {
        match priority {
            RunPriority::Normal => self.default_concurrency,
            RunPriority::Low => self.low_priority_concurrency,
        }
    }

    /// Stop a running run where it is, leaving it resumable.
    pub async fn pause(&self, run_id: Uuid, token: &str) -> Result<(), DomainError> {
        self.control(run_id, token, Verb::Pause).await
    }

    /// Abandon a run. Terminal — a cancelled run is not resumed.
    pub async fn cancel(&self, run_id: Uuid, token: &str) -> Result<(), DomainError> {
        self.control(run_id, token, Verb::Cancel).await
    }

    /// Put a paused run back to work, and tell the caller what to spawn.
    ///
    /// The state machine's one edge back into `running`, and the only verb
    /// that is not a [`Verb`]: it raises no signal — a paused run has no loop
    /// left to read one — and it answers with a value rather than with
    /// nothing. `paused → running` is the whole of its legality; a run
    /// already running has nothing to resume, and the three terminal statuses
    /// have no run left at all.
    ///
    /// Nothing is walked here. The caller spawns `execute` on the returned
    /// [`RunResumed`], and that walk starts over from the root: there is no
    /// cursor to resume from and none is wanted. Per-file work is a stat and
    /// a header read (FR-FC-09/FR-FC-10), so everything an earlier segment
    /// cataloged falls out as `AlreadyCataloged` in seconds, which leaves no
    /// checkpoint to keep honest and no drift to correct.
    ///
    /// `priority` is how a run is re-paced (FR-FC-08 / FR-FC-33). Decision 11
    /// rejected a live throttle slider — `buffer_unordered` fixes its width
    /// when the stream is built — on the promise that pausing and resuming
    /// would do the job instead; this parameter is that promise. `Some`
    /// resolves to a width and *overwrites the run's stored `concurrency`*
    /// before the caller spawns anything, which is what actually re-paces it:
    /// `execute` reads the width off the row (decision 9), so the row is the
    /// only place a new one can be put. `None` keeps the run's own width —
    /// not `Normal`, which would silently speed up every low-priority run
    /// resumed by a caller that predates this parameter.
    pub async fn resume(
        &self,
        run_id: Uuid,
        token: &str,
        priority: Option<RunPriority>,
    ) -> Result<RunResumed, DomainError> {
        // Ahead of the lookup, for the reason `control` does it: a caller
        // with a bad token must learn neither that the run exists nor what
        // state it is in.
        self.auth.authenticate(token).await?;
        let run = self.runs.get(run_id).await?.ok_or(DomainError::NotFound)?;
        if run.status != RunStatus::Paused {
            return Err(DomainError::InvalidState);
        }

        // Bank the pause that is ending. `active_millis` is elapsed wall time
        // minus this total, so a run that sat paused overnight must not
        // report the night as work — and it accumulates rather than replaces,
        // because a run may be paused and resumed any number of times.
        //
        // `paused_at` should always be set on a paused row, and a missing one
        // banks nothing rather than failing the resume: refusing to put a run
        // back to work over a bookkeeping field would be the worse answer,
        // and `active_millis` is clamped at zero regardless.
        let now = self.clock.now();
        let this_pause = run
            .paused_at
            .map(|paused_at| (now - paused_at).num_milliseconds())
            .unwrap_or(0);
        let paused_millis = run.paused_millis + this_pause;

        // The requested width, resolved before the write and handed to it, so
        // the row carries the new number the moment it goes back to
        // `running`. `execute` reads the width off that row (decision 9), and
        // the caller spawns it the instant this returns — so anything later
        // than this write would be a race the resumed segment could lose.
        // `None` binds through as "leave the column alone".
        let requested = priority.map(|priority| self.concurrency_for(priority));
        let applied = retry_on_busy(BUSY_ATTEMPTS, || {
            self.runs.resume(run_id, paused_millis, requested)
        })
        .await?;
        if !applied {
            // The run stopped being `paused` between the lookup above and
            // this write — in practice a cancel that landed in between.
            // Reported rather than swallowed, exactly as a refused pause is:
            // the caller must not be told a run is running again when the row
            // says it was abandoned.
            return Err(DomainError::InvalidState);
        }

        Ok(RunResumed {
            run_id,
            root: run.root,
            kind: run.kind,
            // The width just written, if this resume named one. Failing that,
            // the row's own — and failing *that*, the configured default, for
            // a run started before run priority ever wrote the column. Note
            // this is the value the *caller* is told, not the one `execute`
            // uses: `execute` reads the row. Reporting it anyway keeps
            // `RunResumed` honest about what the run is now paced at, and is
            // what the tests can observe without a walk.
            concurrency: requested
                .or(run.concurrency)
                .unwrap_or(self.default_concurrency),
        })
    }

    async fn control(&self, run_id: Uuid, token: &str, verb: Verb) -> Result<(), DomainError> {
        // AF-02, and first: everything after this point discloses something
        // about the run — whether it exists, and what state it is in. A
        // caller with a bad token must learn neither, so authentication runs
        // ahead of the lookup, not alongside it.
        self.auth.authenticate(token).await?;
        // AF-01: an id naming no run.
        let run = self.runs.get(run_id).await?.ok_or(DomainError::NotFound)?;
        if !verb.permits(run.status) {
            return Err(DomainError::InvalidState);
        }

        if let Some(cell) = self.registry.get(run_id) {
            // This process is executing the run. Raising the signal is the
            // whole job: the walk writes its own row once its in-flight
            // window has drained and its final tally is flushed. Writing the
            // row from here as well would race that write, and the walk's is
            // the one that knows how far it actually got.
            cell.raise(verb.signal());
            return Ok(());
        }

        // No live cell: nothing in this process is executing this run, so
        // there is no loop to write the row and this call has to. In practice
        // that is a `paused` run being cancelled, or the brief window in
        // which a walk has closed its cell but not yet written its own
        // terminal row.
        //
        // Pause is still recorded as a pause here rather than refused or
        // escalated to a cancel: the run has already stopped, so the only
        // question left is what it may become, and `paused` is the answer
        // that keeps the owner's options open. Refusing would leave a
        // `running` row nothing will ever advance, and cancelling would throw
        // away a resume the owner did not ask to give up.
        //
        // About that window, precisely — because the obvious claim, that
        // whichever write lands second wins and both orders are fine, is
        // wrong. A `cancel` landing here while a walk is between dropping its
        // cell and recording its own `pause` must not then be overwritten by
        // that pause: `pause`'s SQL touches neither `finished_at` nor `phase`,
        // so the row would end up `paused` with a finish time already stamped,
        // and a run the owner asked to abandon would look resumable.
        // `RunCell::raise`'s no-downgrade guard cannot help — the cell is
        // already gone. What holds the line is `pause` being conditional on
        // the row still reading `running`, so the late pause is refused and
        // the cancel stands.
        //
        // That covers only the ordering where this call's write lands first.
        // The other one — this call's *lookup* reading `running`, the walk
        // then closing the run, and this call's write landing last — is not
        // covered by the lookup, which has already happened. It is covered by
        // the same shape of guard on each verb's own write: `pause` is
        // conditional on `running`, `cancel` on `running` or `paused`, and
        // both report a refusal below.
        //
        // Unlike the walk's own best-effort bookkeeping, a failure here is
        // reported: the caller asked for the run to stop, and must not be
        // told it did when the row says otherwise.
        let now = self.clock.now();
        match verb {
            Verb::Pause => {
                // `None` for the segment: this caller's subject is the run as
                // the row has it now, whichever execution that is. Matching a
                // segment is a *walk's* guard — it exists so an execution
                // that has already stopped cannot pause the one that replaced
                // it (see `CatalogRunRepository::pause`), and this call holds
                // no stale segment to be wrong about. The status guard below
                // is what covers its own race.
                let applied =
                    retry_on_busy(BUSY_ATTEMPTS, || self.runs.pause(run_id, now, None)).await?;
                if !applied {
                    // The run stopped being `running` between the lookup above
                    // and this write. Reporting the transition as refused is
                    // the honest answer, and the same one the caller would
                    // have got had it arrived a moment later.
                    return Err(DomainError::InvalidState);
                }
            }
            // `None` twice over: this caller holds no partial tally — the
            // walk that does passes its own through `record_halt` — and no
            // segment either, for the reason the pause arm above gives.
            Verb::Cancel => {
                let applied =
                    retry_on_busy(BUSY_ATTEMPTS, || self.runs.cancel(run_id, None, now, None))
                        .await?;
                if !applied {
                    // The run closed itself between the lookup and this
                    // write. Refusing is what keeps a completed run from
                    // being rewritten as `cancelled` with a fresh finish time
                    // while its caller is told `Ok`.
                    return Err(DomainError::InvalidState);
                }
            }
        }
        Ok(())
    }
}
