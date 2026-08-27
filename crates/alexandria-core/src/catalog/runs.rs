use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqlitePool, SqliteRow};
use sqlx::Row;
use uuid::Uuid;

use crate::catalog::index_scope::IndexScope;
use crate::catalog::run_registry::{RunPhase, RunProgress};
use crate::errors::DomainError;

/// How hard a run should push (FR-FC-08). Semantic rather than a raw thread
/// count on purpose: a client starting a big scan should not have to invent a
/// number that happens to be "small," it should be able to say what it wants
/// — keep this out of the way of browsing and playback — and let the server
/// decide what number that means today. `Low` maps to
/// `indexing.low_priority_concurrency` (default 1), `Normal` to the existing
/// `indexing.concurrency` (default 4); which config key a priority means is
/// the handlers' business, not this type's.
///
/// Chosen at `start` *or at `resume`*, and stored on the run's `concurrency`
/// column (`CatalogRun::concurrency`) — not a live slider. `buffer_unordered`
/// fixes its width when the stream is built, so changing your mind mid-run
/// costs a pause and a resume: `RunControlHandler::resume` takes an optional
/// priority, and the run walks its next segment at whatever that resolves to.
/// A resume that names none reuses the width the run already had, which is
/// what every caller predating this option asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RunPriority {
    #[default]
    Normal,
    Low,
}

/// Which command produced a run (FR-FC-27). The two share a lifecycle but not
/// their tallies, which is why `RunCounts` is per-kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunKind {
    Index,
    Refresh,
}

impl RunKind {
    fn as_str(self) -> &'static str {
        match self {
            RunKind::Index => "index",
            RunKind::Refresh => "refresh",
        }
    }

    fn parse(raw: &str) -> Result<Self, DomainError> {
        match raw {
            "index" => Ok(RunKind::Index),
            "refresh" => Ok(RunKind::Refresh),
            other => Err(DomainError::internal(format!("unknown run kind: {other}"))),
        }
    }
}

/// Where a run stands (FR-FC-27, FR-FC-29).
///
/// `Complete` means the walk finished — including when individual files
/// failed, which are counted in the run's own `failed` tally. `Failed` is
/// reserved for a run that could not proceed at all.
///
/// `Paused` is the only non-terminal one of the four: the run stopped where
/// it was and can be picked up again, so it carries no `finished_at`. It is
/// what an owner asks for *and* what startup reconciliation leaves behind for
/// a run whose process stopped (FR-FC-29) — closing the application mid-scan
/// leaves work to resume rather than work to redo, which is why there is no
/// longer an `Interrupted` status naming the loss.
///
/// `Cancelled` is what an owner asks for when they will not come back to a
/// run. Terminal, exactly like `Complete` and `Failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Running,
    Complete,
    Failed,
    Paused,
    Cancelled,
}

impl RunStatus {
    fn as_str(self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Complete => "complete",
            RunStatus::Failed => "failed",
            RunStatus::Paused => "paused",
            RunStatus::Cancelled => "cancelled",
        }
    }

    fn parse(raw: &str) -> Result<Self, DomainError> {
        match raw {
            "running" => Ok(RunStatus::Running),
            "complete" => Ok(RunStatus::Complete),
            "failed" => Ok(RunStatus::Failed),
            "paused" => Ok(RunStatus::Paused),
            "cancelled" => Ok(RunStatus::Cancelled),
            other => Err(DomainError::internal(format!(
                "unknown run status: {other}"
            ))),
        }
    }
}

/// A finished run's tally, mirroring `IndexOutcome` / `RefreshOutcome`.
/// Untagged so it flattens into the run body without a discriminator — `kind`
/// already says which shape to expect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum RunCounts {
    #[serde(rename_all = "camelCase")]
    Index {
        scanned: usize,
        indexed: usize,
        skipped: usize,
        already_cataloged: usize,
        failed: usize,
    },
    #[serde(rename_all = "camelCase")]
    Refresh {
        refreshed: usize,
        marked_missing: usize,
        unchanged: usize,
        failed: usize,
    },
}

/// One recorded index or re-index run (UC-42 / FR-FC-27).
///
/// Fields that do not apply are omitted from the serialized body rather than
/// sent as `null`: a running run carries no counts and no finish time, a
/// refresh carries no root, and only a failed run carries an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogRun {
    #[serde(rename = "runId")]
    pub id: Uuid,
    pub kind: RunKind,
    pub status: RunStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    pub started_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub counts: Option<RunCounts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Which half of the run is executing (FR-FC-28). `None` for a run that
    /// never published one, and `None` again once the run is terminal:
    /// `status = "complete"` alongside `phase = "processing"` would tell a
    /// client two contradictory things. `total` and `processed` survive that
    /// transition — those are the tally, and they stay true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<RunPhase>,
    /// How many entries the run has to get through, once discovery has
    /// counted them. `None` while discovery is still counting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
    /// How many entries the run has finished with — indexed, skipped, and
    /// failed alike. `None` for a run that never published progress.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub processed: Option<usize>,
    /// How long the run has spent *working*: elapsed wall time (to
    /// `finished_at`, or to now for a run still going) minus the time it
    /// spent paused.
    ///
    /// Computed by `GetRunStatusHandler`, which holds the clock — a
    /// repository has no business asking what time it is, and a running run's
    /// elapsed time is not a stored value. Repository implementations leave
    /// this at 0.
    pub active_millis: i64,
    /// When the run was paused, for a run that is paused right now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused_at: Option<DateTime<Utc>>,
    /// Total time the run has spent paused, accumulated across every segment.
    /// Not serialized: it is the input `active_millis` is derived from, and a
    /// client holding `activeMillis` has no use for it.
    #[serde(skip)]
    pub paused_millis: i64,
    /// How many entries at a time the run is being walked, so a resumed run
    /// continues at its own width rather than at whatever the configuration
    /// happens to say later.
    ///
    /// Written by `start`, from the `RunPriority` the caller chose (`Normal`
    /// maps to `indexing.concurrency`, `Low` to
    /// `indexing.low_priority_concurrency`) — see `RunPriority` — and
    /// rewritten by `resume` when that caller names a priority of its own,
    /// which is how a run is re-paced without losing its tally or its record.
    /// `None` only for a run started before run priority existed; a resume of
    /// one of those that names no priority falls back to the configured
    /// default (`RunControlHandler`'s
    /// `default_concurrency`). Not serialized, for the reason `paused_millis`
    /// is not: the run body is what a client draws a progress bar from, and
    /// this is an input to how the run is spawned, not a fact about its
    /// progress.
    #[serde(skip)]
    pub concurrency: Option<u32>,
    /// Which segment of the run is executing: 0 for the one `start` opened,
    /// and one more for every [`resume`].
    ///
    /// A run's identity outlives the walk executing it, and `status` alone
    /// cannot tell a walk that is *still* running from one that is running
    /// *again*. This can. A walk captures the number when it begins and hands
    /// it back to [`pause`], which refuses a write whose segment has moved on
    /// — see that method for the race, which is otherwise unguardable.
    ///
    /// Not serialized, for the reason `concurrency` is not: it says which
    /// execution of the run a caller is looking at, not how far it has got.
    ///
    /// [`resume`]: CatalogRunRepository::resume
    /// [`pause`]: CatalogRunRepository::pause
    #[serde(skip)]
    pub segment: i64,
    /// The file types the run was told to record (FR-FC-01), so a resumed
    /// segment walks the scope the run was started with rather than every
    /// type.
    ///
    /// Recorded for the same reason `root` is: it is what the run was told to
    /// cover, and a resume that could not read it back would catalogue
    /// exactly the files the owner excluded. A NULL column — a refresh, which
    /// has no walk to scope, or a row written before the column existed —
    /// reads back as [`IndexScope::all`], which is what an absent scope has
    /// always meant.
    ///
    /// Not serialized, for the reason `concurrency` is not: it is an input to
    /// how the run is walked, not a fact about its progress.
    #[serde(skip)]
    pub scope: IndexScope,
}

/// Run records repository port (UC-42). Unit-testable against an in-memory
/// fake with no database (Testing Specification §6.2).
#[allow(async_fn_in_trait)]
pub trait CatalogRunRepository: Send + Sync {
    /// Open a run's record as `running` (FR-FC-27). Called by `start()`
    /// before it returns the id it minted, so a started run is always a
    /// recorded run.
    ///
    /// `concurrency` is the width the caller's `RunPriority` resolved to —
    /// see `CatalogRun::concurrency` — recorded here so a resume can reuse it
    /// instead of falling back to whatever the configuration says later.
    ///
    /// `scope` is the run's file types as one comma-separated list of wire
    /// names (`"audio,image"`) — exactly what [`IndexScope::to_wire`]
    /// produces, and exactly what `row_to_run` reads back. `None` is every
    /// type, and is also what a refresh passes: it discovers through the
    /// catalog rather than through a walk, so it has nothing to scope.
    /// Recorded beside `root` because both are what the run was told to
    /// cover, and a resume needs both back.
    async fn start(
        &self,
        id: Uuid,
        kind: RunKind,
        root: Option<&str>,
        started_at: DateTime<Utc>,
        concurrency: u32,
        scope: Option<&str>,
    ) -> Result<(), DomainError>;

    /// Close a run's record as `complete` with its tally. Per-file failures
    /// live inside `counts`; they do not make the run failed.
    ///
    /// Returns `DomainError::internal` if `counts`'s variant does not match
    /// the run's own `RunKind` — writing the wrong variant would leave the
    /// row's real count columns unset, silently masquerading as "no counts
    /// yet" once read back.
    async fn finish(
        &self,
        id: Uuid,
        counts: RunCounts,
        finished_at: DateTime<Utc>,
    ) -> Result<(), DomainError>;

    /// Close a run's record as `failed` — it could not proceed at all.
    async fn fail(
        &self,
        id: Uuid,
        error: &str,
        finished_at: DateTime<Utc>,
    ) -> Result<(), DomainError>;

    /// Stop a run's record short, leaving it resumable.
    ///
    /// The one transition that is *not* terminal, which is why it is the one
    /// that writes neither `finished_at` nor a tally, and the one that leaves
    /// `phase` alone: `finish`, `fail`, and `cancel` clear it because
    /// `status = 'complete'` beside `phase = 'processing'` is a
    /// contradiction, but `status = 'paused'` beside it is exactly the fact a
    /// client needs — the run stopped mid-walk, not mid-discovery.
    ///
    /// `paused_at` is when it stopped: the input a later resume subtracts to
    /// keep `active_millis` honest about time spent working.
    ///
    /// **Conditional on the run still being `running`**, and `Ok(false)` when
    /// it was not. `running` is the only status a pause is ever legal from, so
    /// the condition costs nothing on the legal path — but it is the only
    /// thing standing between a cancel and being silently downgraded. A walk
    /// closes its cell *before* its terminal write, and a `cancel` arriving in
    /// that window finds no live cell, writes `cancelled` directly, and is
    /// then overwritten by the walk's own `pause` — which touches neither
    /// `finished_at` nor `phase`, so the row would end up `paused` with a
    /// finish time already stamped, and a run the owner asked to abandon left
    /// sitting there resumable. The signal's own no-downgrade guard
    /// (`RunCell::raise`) cannot cover this: by then the cell is gone.
    ///
    /// `expected_segment` is the second half of that guard, and the half a
    /// status check cannot supply. `Some(n)` additionally requires the row to
    /// still be on segment `n` — what a *walk* passes, being the segment it
    /// captured when it began. `None` waives it, which is what the control
    /// path passes: a caller acting on the current row means the run as it is
    /// now, whichever segment that is.
    ///
    /// Without it the `running` check is satisfiable by the wrong run. The
    /// walk drops its cell, and *both* a pause and a resume land in the gap
    /// before `record_halt` — the pause finds no live cell and writes the row
    /// itself, a client polls, sees `paused`, and resumes, so a new segment
    /// spawns. The old walk's pause then arrives at a row reading `running`
    /// and applies. The result is a row that says `paused`, with a
    /// `paused_at`, while a segment is actively walking it: `overlay_live_
    /// state` shows a paused run whose `processed` climbs, a launch-time
    /// resume offer lists it, and accepting spawns a *second* concurrent
    /// segment under one run id. `RunCell::raise` cannot help — the cell is
    /// gone — and neither can `status`, which is `running` either way. The
    /// segment is what tells the two apart.
    ///
    /// Callers must not ignore the `false`. See `record_halt`, which logs it,
    /// and `RunControlHandler`, which reports it.
    async fn pause(
        &self,
        id: Uuid,
        paused_at: DateTime<Utc>,
        expected_segment: Option<i64>,
    ) -> Result<bool, DomainError>;

    /// Close a run's record as `cancelled` — the owner abandoned it.
    ///
    /// Terminal, so it stamps `finished_at` and clears `phase` exactly as
    /// `finish` and `fail` do. The progress columns stay: how far a cancelled
    /// run got is still true, and still worth reporting.
    ///
    /// `counts` is the partial tally the walk had reached, and is kept for the
    /// record — a cancelled run is never resumed, so what it got through is
    /// final, and a client deserves the same four numbers a completed run
    /// gives it rather than only `processed`/`total` from the last flush.
    /// `None` is for the caller that has no tally to offer: the control
    /// handler acting on a run no process is executing, where nobody holds a
    /// partial count. (Pause takes no tally for the opposite reason — a paused
    /// run is resumed and re-walks, so its partial tally is superseded rather
    /// than final.)
    ///
    /// Rejects a `counts` whose variant does not match the run's own kind, for
    /// the reason [`CatalogRunRepository::finish`] does. That check is
    /// answered before the state guard below, so a caller passing the wrong
    /// tally is told so rather than quietly getting `Ok(false)`.
    ///
    /// **Conditional on the run's current status**, and `Ok(false)` when the
    /// write was refused — the mirror of [`CatalogRunRepository::pause`]'s
    /// guard, and there for the same race seen from the other verb. A control
    /// call reads `running`, the walk then writes `finish`, and the cancel
    /// lands last: unguarded it rewrites a run that got through all of its
    /// work into a `cancelled` one with a fresh `finished_at`, and answers
    /// `Ok`. The row stays internally coherent, so nothing downstream notices
    /// — it is a misreport rather than a corruption, which is exactly what
    /// makes it worth guarding rather than leaving to be found later.
    ///
    /// What the guard admits depends on whether a tally came with the call,
    /// because the two callers are refusing different things:
    ///
    /// * With `None` — the control handler, holding no tally — `running` and
    ///   `paused`. `paused` is in the set because abandoning a paused run is
    ///   the whole reason to cancel rather than pause.
    /// * With a tally — the walk, recording the cancel it was told to make —
    ///   `cancelled` as well. A control call that found no live cell may have
    ///   written the row already, and letting the walk land on top of it
    ///   replaces `counts: NULL` with the four numbers the walk actually
    ///   computed. The row's status does not change, and its `finished_at`
    ///   moves to when the walk really stopped. Keeping the tally is what the
    ///   design asks of a cancel, and it is only ever lost by refusing here.
    ///
    /// Neither set admits `complete` or `failed`: a run that closed itself is
    /// not one an owner's cancel may rewrite, whichever caller is asking.
    ///
    /// `expected_segment` is the same second half of the guard
    /// [`pause`](CatalogRunRepository::pause) takes, against the same race and
    /// with the same meaning: `Some(n)` from a walk, naming the segment it
    /// captured when it began, and `None` from the control path, whose subject
    /// is the row as it stands. A cancel needs it for the reason a pause does
    /// — a pause and a resume can both land in the gap between a walk dropping
    /// its cell and reaching its terminal write, and `running` is then what the
    /// row reads because a *different* segment is walking it. Cancel is the
    /// worse of the two to get wrong: it is terminal, so the run reads
    /// `cancelled` with a `finished_at` for the whole remaining duration of a
    /// scan that is still going, until the live segment's unconditional
    /// `finish` eventually contradicts it.
    ///
    /// It does not cost the backfill described above, which is why the two
    /// can coexist. A control call cancelling a `running` run does not move
    /// the segment — only [`resume`](CatalogRunRepository::resume) does — so
    /// the walk landing behind it still matches on the segment it captured,
    /// and still replaces `counts: NULL` with its four numbers. What the check
    /// refuses is only the case where a resume *did* intervene, which is
    /// exactly the one where the walk is talking about a run that no longer
    /// exists as it knew it.
    ///
    /// Callers must not ignore the `false`. See `record_halt`, which logs it,
    /// and `RunControlHandler`, which reports it.
    async fn cancel(
        &self,
        id: Uuid,
        counts: Option<RunCounts>,
        cancelled_at: DateTime<Utc>,
        expected_segment: Option<i64>,
    ) -> Result<bool, DomainError>;

    /// Put a paused run back to `running`, ready to be walked again.
    ///
    /// The segment counters are reset rather than carried: `processed` goes
    /// to 0, `total` to NULL, and `phase` back to `discovering`. That is not
    /// housekeeping — it is what keeps resume honest. `processed` counts the
    /// entries one segment folded; it is not a position in the walk, and
    /// there is no position to resume from, because resume re-walks the root
    /// from the start and lets everything an earlier segment cataloged fall
    /// out as `AlreadyCataloged` in seconds. A resumed segment that inherited
    /// the old counters would report a run further along than it is, against
    /// a denominator it has not yet rediscovered.
    ///
    /// `paused_millis` is the *new accumulated total*, computed by the caller:
    /// the repository has no business asking what time it is, exactly as it
    /// does not for `active_millis`. The caller adds the pause that is ending
    /// to whatever was already banked and passes the sum.
    ///
    /// `concurrency` re-paces the run: `Some(width)` overwrites the stored
    /// column so the segment about to be spawned walks at the new width;
    /// `None` leaves whatever is there alone, which is what a resume that
    /// named no priority means. It is written *here*, in the same statement
    /// as the status change, rather than by a separate call: `execute` reads
    /// the width back off this row (decision 9), so a resume that flipped the
    /// status first and the width second would leave a window in which the
    /// spawned segment could read the old number.
    ///
    /// **Conditional on the run still being `paused`**, and `Ok(false)` when
    /// it was not — the same guard `pause` and `cancel` carry. A resume that
    /// landed after someone else's cancel would revive a run its owner had
    /// just abandoned. The new width rides along inside that guard: a refused
    /// resume must not re-pace the run it failed to revive.
    ///
    /// Also inside it, and for the same reason: `segment` is incremented. The
    /// run is going back to work as a new execution, and that number is what
    /// lets the *previous* one's late writes be told apart from this one's —
    /// see [`pause`]. A refused resume must not bump it, or it would invalidate
    /// a live walk's pause without ever having started a walk of its own.
    ///
    /// [`pause`]: CatalogRunRepository::pause
    async fn resume(
        &self,
        id: Uuid,
        paused_millis: i64,
        concurrency: Option<u32>,
    ) -> Result<bool, DomainError>;

    /// Flush a run's live progress into its record (FR-FC-28).
    ///
    /// Called periodically while the run executes, not once per entry: the
    /// in-memory cell is authoritative for a live run, and this write only
    /// exists so a run this process is no longer executing can still report
    /// how far it got. A failure is therefore not fatal — see the handlers,
    /// which log it and carry on.
    async fn record_progress(&self, id: Uuid, progress: &RunProgress) -> Result<(), DomainError>;

    /// One run's record, or `None` for an unknown id (UC-42 AF-01).
    async fn get(&self, id: Uuid) -> Result<Option<CatalogRun>, DomainError>;

    /// Every run whose status is `running` or `paused` — the two non-terminal
    /// statuses (see [`RunStatus`]) — newest first.
    ///
    /// This is what answers "is anything indexing right now" and "what can be
    /// resumed" across the whole catalog in one call, rather than the caller
    /// having to remember every run id it ever started and poll [`get`] on
    /// each — see `GetActiveRunsHandler`. An idle library has none: that is
    /// an empty list, not an error.
    ///
    /// Newest first because that is the order either question wants an
    /// answer read in: a resume offer leads with the run most likely to still
    /// matter to the owner, and an activity indicator built from the first
    /// entry wants the freshest one.
    ///
    /// [`get`]: CatalogRunRepository::get
    async fn list_active(&self) -> Result<Vec<CatalogRun>, DomainError>;

    /// Pause every run still `running`, returning how many were reconciled
    /// (FR-FC-29).
    ///
    /// Runs execute in-process, so a `running` row seen at startup provably
    /// has no task behind it. It becomes `paused` rather than terminal: the
    /// walk it belongs to can be resumed, and closing the application
    /// mid-scan should leave an owner a run they are offered rather than a
    /// loss they are informed of. Nothing is started here — resuming is an
    /// explicit act.
    ///
    /// Stamps `paused_at` like any other pause, so the time the process spent
    /// down is banked and not later reported as work, and writes no
    /// `finished_at`, because the run has not finished.
    ///
    /// Unlike an owner's pause, this one clears `phase`. A paused run keeps
    /// its phase to say where its still-live walk stopped; this run's process
    /// is gone, so it is not in a phase at all until it resumes.
    async fn pause_running(&self, now: DateTime<Utc>) -> Result<u64, DomainError>;
}

#[derive(Clone)]
pub struct SqliteCatalogRunRepository {
    pool: SqlitePool,
}

impl SqliteCatalogRunRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

/// Parse an RFC 3339 column into a `DateTime<Utc>`; a corrupt value is an
/// internal error rather than a silent default.
fn parse_time(raw: &str, column: &str) -> Result<DateTime<Utc>, DomainError> {
    DateTime::parse_from_rfc3339(raw)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|err| DomainError::internal(format!("corrupt catalog_runs.{column}: {err}")))
}

/// Every `catalog_runs` column [`row_to_run`] needs, other than `id` — `get`
/// already has the id it queried by and has no reason to ask the row for it
/// back, while `list_active` has no id to bind against and must select it.
/// A macro rather than a `const &str`: `sqlx::query` requires a `&'static
/// str` it can statically audit for injection, which rules out building the
/// two queries below with `format!` — this expands inline at compile time
/// instead, so the column list is still written once.
macro_rules! run_columns {
    () => {
        "kind, status, root, started_at, finished_at, scanned, indexed, \
         skipped, already_cataloged, refreshed, marked_missing, unchanged, failed, error, \
         phase, total, processed, paused_at, paused_millis, concurrency, segment, scope"
    };
}

/// `get`'s query: one row by id, or none.
const GET_RUN_QUERY: &str = concat!("SELECT ", run_columns!(), " FROM catalog_runs WHERE id = ?");

/// `list_active`'s query (see [`CatalogRunRepository::list_active`]):
/// `running` and `paused` are the two non-terminal statuses, and
/// `ORDER BY started_at DESC` is explicit rather than left to whatever order
/// SQLite happens to return rows in — it makes no promise without one.
const LIST_ACTIVE_RUNS_QUERY: &str = concat!(
    "SELECT id, ",
    run_columns!(),
    " FROM catalog_runs WHERE status IN ('running', 'paused') ORDER BY started_at DESC"
);

/// Map one `catalog_runs` row into a [`CatalogRun`], given the id the row was
/// found by (`get`) or read out of (`list_active`). Shared so the two query
/// paths cannot drift on how a row's columns become a run.
fn row_to_run(id: Uuid, row: &SqliteRow) -> Result<CatalogRun, DomainError> {
    let kind = RunKind::parse(&row.try_get::<String, _>("kind")?)?;
    let status = RunStatus::parse(&row.try_get::<String, _>("status")?)?;
    let started_at = parse_time(&row.try_get::<String, _>("started_at")?, "started_at")?;
    let finished_at = row
        .try_get::<Option<String>, _>("finished_at")?
        .map(|raw| parse_time(&raw, "finished_at"))
        .transpose()?;

    // Counts exist only once a walk has finished. Presence of the first
    // column of the kind's set decides — `finish` writes them together.
    // The reverse narrowing: these columns hold file counts a single walk
    // produced, so a stored value large enough to overflow `usize` on a
    // 32-bit target is not reachable, and the cast is not checked.
    let counts = match kind {
        RunKind::Index => row
            .try_get::<Option<i64>, _>("scanned")?
            .map(|scanned| -> Result<RunCounts, DomainError> {
                Ok(RunCounts::Index {
                    scanned: scanned as usize,
                    indexed: row.try_get::<i64, _>("indexed")? as usize,
                    skipped: row.try_get::<i64, _>("skipped")? as usize,
                    already_cataloged: row.try_get::<i64, _>("already_cataloged")? as usize,
                    failed: row.try_get::<i64, _>("failed")? as usize,
                })
            })
            .transpose()?,
        RunKind::Refresh => row
            .try_get::<Option<i64>, _>("refreshed")?
            .map(|refreshed| -> Result<RunCounts, DomainError> {
                Ok(RunCounts::Refresh {
                    refreshed: refreshed as usize,
                    marked_missing: row.try_get::<i64, _>("marked_missing")? as usize,
                    unchanged: row.try_get::<i64, _>("unchanged")? as usize,
                    failed: row.try_get::<i64, _>("failed")? as usize,
                })
            })
            .transpose()?,
    };

    // The last flushed progress (FR-FC-28). A stored `phase` that parses
    // to nothing is dropped rather than failing the read: progress is a
    // display field, and refusing to answer at all would be a worse
    // outcome than answering without it.
    let phase = row
        .try_get::<Option<String>, _>("phase")?
        .as_deref()
        .and_then(RunPhase::parse);
    let paused_at = row
        .try_get::<Option<String>, _>("paused_at")?
        .map(|raw| parse_time(&raw, "paused_at"))
        .transpose()?;

    Ok(CatalogRun {
        id,
        kind,
        status,
        root: row.try_get("root")?,
        started_at,
        finished_at,
        counts,
        error: row.try_get("error")?,
        phase,
        total: row
            .try_get::<Option<i64>, _>("total")?
            .map(|total| total as usize),
        processed: row
            .try_get::<Option<i64>, _>("processed")?
            .map(|processed| processed as usize),
        // Derived by `GetRunStatusHandler` / `GetActiveRunsHandler`, which
        // hold the clock — a repository has no business asking what time it
        // is.
        active_millis: 0,
        paused_at,
        paused_millis: row.try_get("paused_millis")?,
        // The reverse narrowing of `start`'s: a concurrency wider than
        // `u32` was never written, so the cast cannot lose anything.
        concurrency: row
            .try_get::<Option<i64>, _>("concurrency")?
            .map(|concurrency| concurrency as u32),
        segment: row.try_get("segment")?,
        // A corrupt value is reported rather than guessed at, the way
        // `parse_type_str` reports an unknown `files.type`: the only fallback
        // available is "every type", which is precisely the behaviour a
        // stored scope exists to prevent.
        scope: match row.try_get::<Option<String>, _>("scope")? {
            Some(raw) => IndexScope::parse_list(&raw).map_err(|err| {
                DomainError::internal(format!("corrupt catalog_runs.scope: {err}"))
            })?,
            None => IndexScope::all(),
        },
    })
}

/// Reject a `finish()` call whose `RunCounts` variant does not match the
/// run's own kind. Writing the wrong variant would silently leave the row's
/// real count columns NULL — this turns that into a loud error instead.
fn check_counts_match_kind(kind: RunKind, counts: &RunCounts) -> Result<(), DomainError> {
    let matches = matches!(
        (kind, counts),
        (RunKind::Index, RunCounts::Index { .. }) | (RunKind::Refresh, RunCounts::Refresh { .. })
    );
    if matches {
        Ok(())
    } else {
        Err(DomainError::internal(format!(
            "counts kind mismatch: run is {:?} but counts are {:?}",
            kind, counts
        )))
    }
}

/// The `WHERE` suffix that makes a close conditional on the row still being
/// cancellable. A macro rather than a `const` because it is `concat!`ed into
/// the statements below, and `concat!` takes literals only.
macro_rules! cancellable_guard {
    () => {
        " AND status IN (?, ?)"
    };
}

/// The same for a cancel that carries a tally, whose set is one wider. See
/// [`TALLY_CANCELLABLE_FROM`].
macro_rules! tally_cancellable_guard {
    () => {
        " AND status IN (?, ?, ?)"
    };
}

/// The clause that pins a halt to the segment the caller was executing, for
/// the statements that take one. Shared by `pause` and both `cancel` forms,
/// which face the identical race and must not drift on how they answer it.
///
/// A bound `NULL` waives it — the control path, whose subject is the row as it
/// stands. `Some(n)` requires the row to still be on segment `n`, which is how
/// a walk says "the run I was walking", not "whatever this run is now". Two
/// binds of the one value rather than a numbered parameter: sqlx counts
/// positional `?`s, and mixing the two forms is how that goes wrong.
macro_rules! segment_guard {
    () => {
        " AND (? IS NULL OR segment = ?)"
    };
}

/// The statement that closes an index run with its tally, with `$guard`
/// appended.
///
/// `phase = NULL` because the run is terminal: a row reading
/// `status = 'complete', phase = 'processing'` tells a client two
/// contradictory things. `total` and `processed` stay — those are the tally,
/// and they remain true.
macro_rules! close_index_sql {
    ($guard:expr) => {
        concat!(
            "UPDATE catalog_runs SET status = ?, finished_at = ?, phase = NULL, \
             scanned = ?, indexed = ?, skipped = ?, already_cataloged = ?, failed = ? \
             WHERE id = ?",
            $guard
        )
    };
}

/// The same for a refresh run's tally. `phase = NULL`: terminal, as above.
macro_rules! close_refresh_sql {
    ($guard:expr) => {
        concat!(
            "UPDATE catalog_runs SET status = ?, finished_at = ?, phase = NULL, \
             refreshed = ?, marked_missing = ?, unchanged = ?, failed = ? \
             WHERE id = ?",
            $guard
        )
    };
}

/// The statuses a tally-less `cancel` may be written from — the control
/// handler's. See the trait doc: `paused` is in the set because abandoning a
/// paused run is the whole reason to cancel rather than pause.
const CANCELLABLE_FROM: [RunStatus; 2] = [RunStatus::Running, RunStatus::Paused];

/// The statuses a `cancel` *carrying a tally* may be written from — the
/// walk's. [`CANCELLABLE_FROM`] plus `cancelled`, so a walk landing behind a
/// control call that already wrote the row replaces its empty tally with the
/// four numbers the walk computed rather than dropping them. See the trait
/// doc.
const TALLY_CANCELLABLE_FROM: [RunStatus; 3] =
    [RunStatus::Running, RunStatus::Paused, RunStatus::Cancelled];

impl SqliteCatalogRunRepository {
    /// Close a run terminally with its tally.
    ///
    /// Shared by `finish` and `cancel`, which differ only in the status they
    /// write and in what they will write it over: both are terminal, both
    /// stamp `finished_at`, both clear `phase`, and both keep the four counts.
    /// Sharing it is what stops the two from drifting on any of that.
    ///
    /// `guarded` says whether the write is conditional on the row still being
    /// cancellable *with a tally* — [`TALLY_CANCELLABLE_FROM`], the only set
    /// this method is ever asked for, since the other one belongs to the
    /// branch that has no counts to write. `finish` is unconditional: it is
    /// the walk recording its own completion, and there is no writer it
    /// should lose to. `cancel` is guarded; the trait doc says why. Returns
    /// whether the write applied, which is only ever `false` for a guarded
    /// one.
    ///
    /// `expected_segment` rides inside that same guard when there is one —
    /// see [`segment_guard`] and [`CatalogRunRepository::cancel`]. It is
    /// bound only for a guarded write; an unguarded one has nothing to lose
    /// to and no clause to bind it to.
    async fn close_with_counts(
        &self,
        id: Uuid,
        status: RunStatus,
        counts: RunCounts,
        finished_at: DateTime<Utc>,
        guarded: bool,
        expected_segment: Option<i64>,
    ) -> Result<bool, DomainError> {
        // Guard against a caller passing the wrong kind's tally: writing it
        // would leave the row's own kind's columns NULL, and `get` would then
        // report a terminal run with no counts — a corrupted write that looks
        // like "no counts yet" instead of failing loudly. Answered ahead of
        // the state guard below, so a caller that passed the wrong tally is
        // told so rather than handed a plain `false`.
        let row = sqlx::query("SELECT kind FROM catalog_runs WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        if let Some(row) = row {
            let kind = RunKind::parse(&row.try_get::<String, _>("kind")?)?;
            check_counts_match_kind(kind, &counts)?;
        }

        // Four literal statements rather than one assembled at runtime: sqlx
        // takes a `&'static str`, and dynamic SQL is something this
        // repository has never had. The macros are what keep the guarded and
        // unguarded form of each from drifting in anything but the guard.
        let sql = match (&counts, guarded) {
            (RunCounts::Index { .. }, false) => close_index_sql!(""),
            (RunCounts::Index { .. }, true) => {
                close_index_sql!(concat!(tally_cancellable_guard!(), segment_guard!()))
            }
            (RunCounts::Refresh { .. }, false) => close_refresh_sql!(""),
            (RunCounts::Refresh { .. }, true) => {
                close_refresh_sql!(concat!(tally_cancellable_guard!(), segment_guard!()))
            }
        };
        let query = sqlx::query(sql)
            .bind(status.as_str())
            .bind(finished_at.to_rfc3339());
        let query = match counts {
            // These are file counts from a single walk; a library large
            // enough to overflow `i64` is not reachable, so the narrowing is
            // not checked at runtime.
            RunCounts::Index {
                scanned,
                indexed,
                skipped,
                already_cataloged,
                failed,
            } => query
                .bind(scanned as i64)
                .bind(indexed as i64)
                .bind(skipped as i64)
                .bind(already_cataloged as i64)
                .bind(failed as i64),
            RunCounts::Refresh {
                refreshed,
                marked_missing,
                unchanged,
                failed,
            } => query
                .bind(refreshed as i64)
                .bind(marked_missing as i64)
                .bind(unchanged as i64)
                .bind(failed as i64),
        };
        let query = query.bind(id.to_string());
        // Bound after the id, matching the order the placeholders appear in.
        let query = if guarded {
            let query = TALLY_CANCELLABLE_FROM
                .iter()
                .fold(query, |query, status| query.bind(status.as_str()));
            // Last, matching where `segment_guard!()` is concatenated.
            query.bind(expected_segment).bind(expected_segment)
        } else {
            query
        };
        let result = query.execute(&self.pool).await?;
        Ok(result.rows_affected() > 0)
    }
}

impl CatalogRunRepository for SqliteCatalogRunRepository {
    async fn start(
        &self,
        id: Uuid,
        kind: RunKind,
        root: Option<&str>,
        started_at: DateTime<Utc>,
        concurrency: u32,
        scope: Option<&str>,
    ) -> Result<(), DomainError> {
        sqlx::query(
            "INSERT INTO catalog_runs (id, kind, status, root, started_at, concurrency, scope) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(kind.as_str())
        .bind(RunStatus::Running.as_str())
        .bind(root)
        .bind(started_at.to_rfc3339())
        // Narrowed to i64 for the column; the reverse narrowing happens in
        // `get`, which is safe for the reason documented there.
        .bind(concurrency as i64)
        // The wire list `IndexScope::to_wire` produced, or NULL for every
        // type — see `CatalogRunRepository::start`.
        .bind(scope)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn finish(
        &self,
        id: Uuid,
        counts: RunCounts,
        finished_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        // Unconditional: this is the walk recording its own completion, and
        // there is no writer it should lose to. `None` for the segment
        // follows from that — an unguarded write binds no segment clause.
        self.close_with_counts(id, RunStatus::Complete, counts, finished_at, false, None)
            .await?;
        Ok(())
    }

    async fn fail(
        &self,
        id: Uuid,
        error: &str,
        finished_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        sqlx::query(
            "UPDATE catalog_runs SET status = ?, finished_at = ?, error = ?, phase = NULL \
             WHERE id = ?",
        )
        .bind(RunStatus::Failed.as_str())
        .bind(finished_at.to_rfc3339())
        .bind(error)
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn pause(
        &self,
        id: Uuid,
        paused_at: DateTime<Utc>,
        expected_segment: Option<i64>,
    ) -> Result<bool, DomainError> {
        // No `phase = NULL` here, unlike every other transition below and
        // above: a paused run is not terminal, so its phase is not a
        // contradiction but the very thing that says where it stopped.
        // No `finished_at` either — it has not finished.
        //
        // `AND status = 'running'` is the guard the trait doc explains: a
        // pause is only ever legal from `running`, and without the condition
        // this write would overwrite a `cancelled` row that landed while the
        // walk was between closing its cell and recording itself — leaving
        // `paused` beside a `finished_at` the pause never wrote, and a run the
        // owner abandoned looking resumable.
        //
        // [`segment_guard`] is the other half, and the half `status` cannot
        // express: `running` is also what the row reads when somebody paused
        // *and resumed* it inside that same window, and a walk's late pause
        // landing on the resumed segment stops nothing and misreports
        // everything. See the trait doc. Both `cancel` forms carry the same
        // clause, against the same race.
        let result = sqlx::query(concat!(
            "UPDATE catalog_runs SET status = ?, paused_at = ? \
             WHERE id = ? AND status = ?",
            segment_guard!()
        ))
        .bind(RunStatus::Paused.as_str())
        .bind(paused_at.to_rfc3339())
        .bind(id.to_string())
        .bind(RunStatus::Running.as_str())
        .bind(expected_segment)
        .bind(expected_segment)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn cancel(
        &self,
        id: Uuid,
        counts: Option<RunCounts>,
        cancelled_at: DateTime<Utc>,
        expected_segment: Option<i64>,
    ) -> Result<bool, DomainError> {
        let Some(counts) = counts else {
            // No tally on offer — the control handler acting on a run no
            // process is executing. `phase = NULL`: terminal, exactly as in
            // `finish` and `fail`. The `status IN (…)` guard is the trait
            // doc's: a cancel must not rewrite a run that closed itself
            // between this caller's lookup and this write.
            //
            // [`segment_guard`] joins it here as it does on `pause`. This
            // caller binds `NULL` in practice, but the clause is on the
            // statement rather than on the caller so the two `cancel` forms
            // cannot drift on what they guard against.
            let result = sqlx::query(concat!(
                "UPDATE catalog_runs SET status = ?, finished_at = ?, phase = NULL \
                 WHERE id = ?",
                cancellable_guard!(),
                segment_guard!()
            ))
            .bind(RunStatus::Cancelled.as_str())
            .bind(cancelled_at.to_rfc3339())
            .bind(id.to_string())
            .bind(CANCELLABLE_FROM[0].as_str())
            .bind(CANCELLABLE_FROM[1].as_str())
            .bind(expected_segment)
            .bind(expected_segment)
            .execute(&self.pool)
            .await?;
            return Ok(result.rows_affected() > 0);
        };
        // A cancelled run is never resumed, so the tally it reached is final —
        // it is kept for exactly the reason a completed run's is.
        self.close_with_counts(
            id,
            RunStatus::Cancelled,
            counts,
            cancelled_at,
            true,
            expected_segment,
        )
        .await
    }

    async fn resume(
        &self,
        id: Uuid,
        paused_millis: i64,
        concurrency: Option<u32>,
    ) -> Result<bool, DomainError> {
        // One statement, so a resume either happens completely or not at all:
        // a row left `running` with a stale `paused_at` would go on banking
        // the same pause on every subsequent resume.
        //
        // `processed = 0, total = NULL, phase = 'discovering'` puts the run
        // back where every run starts. See the trait doc — those counters
        // describe a segment, not a position, and resume re-walks.
        //
        // `concurrency = COALESCE(?, concurrency)` is how the new width joins
        // that same statement: a bound `NULL` — a resume that named no
        // priority — leaves the column exactly as it was, including leaving a
        // pre-column run's `NULL` alone, while a bound width overwrites it.
        // Expressing "keep" as a no-op inside the one guarded UPDATE is what
        // keeps a refused resume from re-pacing a run it did not revive.
        //
        // `segment = segment + 1` rides in the same statement for the same
        // reason, and it is what makes the walk about to be spawned
        // distinguishable from the one that just stopped: the previous
        // segment's late `pause` is matched out by it. A refused resume must
        // not bump it either — that would invalidate a live walk's pause
        // without starting a walk to replace it.
        let result = sqlx::query(
            "UPDATE catalog_runs SET status = ?, paused_at = NULL, paused_millis = ?, \
             processed = 0, total = NULL, phase = ?, segment = segment + 1, \
             concurrency = COALESCE(?, concurrency) WHERE id = ? AND status = ?",
        )
        .bind(RunStatus::Running.as_str())
        .bind(paused_millis)
        .bind(RunPhase::Discovering.as_str())
        // The same widening `start` does — SQLite has no unsigned integer, and
        // a width is never large enough for the cast to lose anything.
        .bind(concurrency.map(|concurrency| concurrency as i64))
        .bind(id.to_string())
        .bind(RunStatus::Paused.as_str())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn record_progress(&self, id: Uuid, progress: &RunProgress) -> Result<(), DomainError> {
        sqlx::query("UPDATE catalog_runs SET phase = ?, total = ?, processed = ? WHERE id = ?")
            .bind(progress.phase.as_str())
            // File counts from a single walk; a library large enough to
            // overflow `i64` is not reachable, so the narrowing is unchecked
            // exactly as it is in `finish`.
            .bind(progress.total.map(|total| total as i64))
            .bind(progress.processed as i64)
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn get(&self, id: Uuid) -> Result<Option<CatalogRun>, DomainError> {
        let row = sqlx::query(GET_RUN_QUERY)
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        Ok(Some(row_to_run(id, &row)?))
    }

    async fn list_active(&self) -> Result<Vec<CatalogRun>, DomainError> {
        // `started_at DESC` is explicit rather than relied upon from
        // insertion order — SQLite makes no ordering promise without an
        // ORDER BY, and `catalog_runs` has no autoincrement rowid ordering a
        // caller could accidentally lean on either (the primary key is the
        // run's own UUID).
        let rows = sqlx::query(LIST_ACTIVE_RUNS_QUERY)
            .fetch_all(&self.pool)
            .await?;

        rows.iter()
            .map(|row| {
                let raw_id = row.try_get::<String, _>("id")?;
                let id = Uuid::parse_str(&raw_id).map_err(|err| {
                    DomainError::internal(format!("corrupt catalog_runs.id {raw_id:?}: {err}"))
                })?;
                row_to_run(id, row)
            })
            .collect()
    }

    async fn pause_running(&self, now: DateTime<Utc>) -> Result<u64, DomainError> {
        // No `finished_at`: this run is being offered for resume, not closed.
        // `phase = NULL` because its process is gone — see the trait doc for
        // why this is the one pause that clears it.
        let result = sqlx::query(
            "UPDATE catalog_runs SET status = ?, paused_at = ?, phase = NULL WHERE status = ?",
        )
        .bind(RunStatus::Paused.as_str())
        .bind(now.to_rfc3339())
        .bind(RunStatus::Running.as_str())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}
