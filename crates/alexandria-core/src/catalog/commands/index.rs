use chrono::{DateTime, Utc};
use futures_util::stream::{self, StreamExt};
use serde::Serialize;
use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::audio_tags::AudioMetadataReader;
use crate::catalog::classify::classify_by_extension;
use crate::catalog::clock::Clock;
use crate::catalog::comic_tags::ComicMetadataReader;
use crate::catalog::commands::{flush_progress, record_halt, PROGRESS_FLUSH_SECONDS};
use crate::catalog::document_tags::DocumentMetadataReader;
use crate::catalog::fs::{FileEntry, Filesystem};
use crate::catalog::image_tags::ImageMetadataReader;
use crate::catalog::index_scope::IndexScope;
use crate::catalog::model::{FileType, NewFile};
use crate::catalog::repos::CatalogRepository;
use crate::catalog::run_registry::{RunCell, RunPhase, RunRegistry, RunSignal};
use crate::catalog::runs::{CatalogRunRepository, RunCounts, RunKind, RunPriority};
use crate::catalog::video_tags::VideoMetadataReader;
use crate::errors::DomainError;
use crate::retry::{retry_on_busy, BUSY_ATTEMPTS};

#[derive(Debug, Clone)]
pub struct IndexRequest {
    pub root: String,
    /// How hard this run should push (FR-FC-08). See `RunPriority`.
    pub priority: RunPriority,
    /// The file types this run records. Default is every supported type, so a
    /// caller that says nothing indexes what it always did. See `IndexScope`
    /// for why the scope travels to `execute` rather than onto the run row.
    pub scope: IndexScope,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexStarted {
    #[serde(rename = "runId")]
    pub run_id: Uuid,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexOutcome {
    #[serde(rename = "runId")]
    pub run_id: Uuid,
    pub scanned: usize,
    pub indexed: usize,
    pub skipped: usize,
    /// Entries whose path was already in the catalog (AF-03) — distinct from
    /// `skipped`, which is an unsupported extension. Resume re-walks a root
    /// and re-encounters everything an earlier segment already indexed, so
    /// without this split a resumed run would report thousands of files as
    /// `skipped`, a tally that misdescribes what happened.
    pub already_cataloged: usize,
    /// Entries that could not be indexed because an operation against that one
    /// file failed (unreadable bytes, or a repository write error). The run
    /// continues past them; each is logged at `warn`.
    pub failed: usize,
}

/// Index library files (UC-01).
///
/// `start` authenticates the caller, validates the root path — it must exist,
/// and it must sit inside the configured `filesystem.root` when one is set
/// (FR-FC-26) — and returns a fresh run id immediately. The heavy `execute`
/// walk stats, classifies, and persists each supported file, skipping
/// already-cataloged paths — no file bytes are read (FR-FC-09/FR-FC-10). `start` and `execute` are separated so the HTTP/FFI layer can spawn
/// `execute` in the background (FR-FC-08) while `start` returns `202` right
/// away.
///
/// `execute` processes up to `concurrency` files at a time, where
/// `concurrency` is the width the run's own `RunPriority` resolved to at
/// `start` (`indexing.concurrency` default 4 for `Normal`,
/// `indexing.low_priority_concurrency` default 1 for `Low` — FR-FC-08) and is
/// read back from the run's stored `concurrency` column rather than from a
/// field, so a resumed run walks at whatever width the run currently carries
/// — the one it was started with, or the one a resume re-paced it to
/// (`RunControlHandler::resume`). The per-file work is a stat plus, for a
/// supported extension, a tag-header read — never a full-file hash
/// (FR-FC-09/FR-FC-10) — and `StdFilesystem` runs both on Tokio's blocking
/// pool, so the concurrency still buys real parallelism rather than
/// interleaved waiting. NFR-02's throughput target rests on the per-file cost
/// being size-independent, which is what dropping the hash bought; the
/// concurrency is what keeps the syscalls from serializing. It is bounded
/// rather than unlimited because an unbounded fan-out over a large library
/// would queue one blocking task per file and starve every other user of the
/// blocking pool. Note that the *database* half of
/// each file's work still serializes: SQLite admits one writer at a time, and
/// the pool caps connections at 8, so raising `concurrency` past that only
/// lengthens the queue in front of the writer.
///
/// Generic over its collaborators so the same decision logic is unit-tested
/// against trait fakes (no real DB, filesystem, or auth service in unit
/// tests), then wired with the concrete Sqlite/StdFilesystem/Bearer/services
/// at runtime.
pub struct IndexHandler<A, R, F, C, M, N, O, P, Q, RR> {
    auth: A,
    repo: R,
    fs: F,
    clock: C,
    audio_tags: M,
    image_tags: N,
    document_tags: O,
    video_tags: P,
    comic_tags: Q,
    concurrency: usize,
    /// The width a `RunPriority::Low` run processes at
    /// (`indexing.low_priority_concurrency`). See `concurrency` for the
    /// `Normal` counterpart and the zero clamp both share.
    low_priority_concurrency: usize,
    /// The configured library root (`filesystem.root`) every requested index
    /// root must sit inside (FR-FC-26). `None` when the key is unset, which
    /// leaves indexing unconstrained — the historical behaviour.
    library_root: Option<String>,
    runs: RR,
    /// Where `execute` publishes this run's live progress (FR-FC-28). Shared
    /// with `GetRunStatusHandler`, which reads it back.
    registry: RunRegistry,
}

/// The client-facing rejection message for FR-FC-26 when the *requested*
/// root is genuinely outside the configured library root. Deliberately free
/// of the configured root's absolute path: the caller does not need to be
/// told where the library lives in order to learn that its request was out
/// of bounds.
const OUTSIDE_LIBRARY_ROOT: &str = "root path is outside the configured library root";

/// The client-facing rejection message for FR-FC-26 when the *server's*
/// `filesystem.root` configuration itself cannot be resolved. Deliberately
/// distinct from [`OUTSIDE_LIBRARY_ROOT`]: that message implies the caller's
/// request was wrong, which is misleading here — the caller's root may be
/// perfectly fine, and it is the server's configuration that needs fixing.
/// Still free of the configured root's absolute path — naming the failure
/// mode is not the same as naming the path.
const LIBRARY_ROOT_UNRESOLVABLE: &str =
    "the server's configured library root could not be resolved; contact the operator";

/// What one scanned entry resolved to. Returned by the per-entry future so
/// the concurrent walk can tally outcomes without sharing a counter.
///
/// `Skipped` and `AlreadyCataloged` are two different facts and were one
/// counter until resume existed. A resumed run re-walks and re-skips
/// everything a previous segment indexed, so folding the two together made a
/// resumed run report thousands of files as "skipped" — a tally that
/// misdescribes what happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryOutcome {
    Indexed,
    Skipped,
    AlreadyCataloged,
    Failed,
    /// The run was paused or cancelled before this entry did any work.
    ///
    /// It contributes to no counter, and does not advance `processed`
    /// either — the run never touched it. That deliberately breaks the tally
    /// invariant the other four keep: for a halted run, `scanned` exceeds
    /// `indexed + skipped + already_cataloged + failed`, because the
    /// difference is the entries that were never processed. Counting them
    /// anywhere would be worse — it would tell a client, and Task 8's
    /// resume, that the run got through files it never opened.
    Halted,
}

/// What `execute`'s processing loop carries from one entry to the next: the
/// outcome tally, and when it last flushed progress.
///
/// `last_flush` rides along in the fold's accumulator rather than sitting in a
/// `Cell` outside it because the resulting future has to be `Send` — the
/// HTTP and FFI layers spawn `execute` onto the runtime — and a shared
/// `Cell` is not.
struct IndexTally {
    indexed: usize,
    skipped: usize,
    already_cataloged: usize,
    failed: usize,
    last_flush: DateTime<Utc>,
}

impl IndexTally {
    /// `started` is when the processing loop began — after discovery and
    /// after the unconditional flush that ends it — so the first interval
    /// flush lands one interval into the loop rather than on its first entry.
    fn new(started: DateTime<Utc>) -> Self {
        Self {
            indexed: 0,
            skipped: 0,
            already_cataloged: 0,
            failed: 0,
            last_flush: started,
        }
    }
}

impl<A, R, F, C, M, N, O, P, Q, RR> IndexHandler<A, R, F, C, M, N, O, P, Q, RR>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
    C: Clock,
    M: AudioMetadataReader,
    N: ImageMetadataReader,
    O: DocumentMetadataReader,
    P: VideoMetadataReader,
    Q: ComicMetadataReader,
    RR: CatalogRunRepository,
{
    /// `concurrency` is how many files `execute` processes at a time for a
    /// `RunPriority::Normal` run (`indexing.concurrency`);
    /// `low_priority_concurrency` is the same for `RunPriority::Low`
    /// (`indexing.low_priority_concurrency`). Zero is meaningless for either —
    /// a stream buffered zero deep makes no progress — so both are clamped to
    /// 1, which is the sequential behaviour a caller asking for "no
    /// concurrency" means.
    ///
    /// `library_root` is the configured `filesystem.root` (FR-FC-26). An
    /// empty string means the key is unset, and indexing stays unconstrained
    /// exactly as it was before the constraint existed — the constraint is
    /// opt-in by configuration, so no existing deployment changes behaviour
    /// on upgrade.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        auth: A,
        repo: R,
        fs: F,
        clock: C,
        audio_tags: M,
        image_tags: N,
        document_tags: O,
        video_tags: P,
        comic_tags: Q,
        concurrency: u32,
        low_priority_concurrency: u32,
        library_root: String,
        runs: RR,
        registry: RunRegistry,
    ) -> Self {
        let library_root = {
            let trimmed = library_root.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        };
        Self {
            auth,
            repo,
            fs,
            clock,
            audio_tags,
            image_tags,
            document_tags,
            video_tags,
            comic_tags,
            concurrency: concurrency.max(1) as usize,
            low_priority_concurrency: low_priority_concurrency.max(1) as usize,
            library_root,
            runs,
            registry,
        }
    }

    /// The width a run at `priority` should be walked at — `Normal` maps to
    /// `concurrency`, `Low` to `low_priority_concurrency`. See `RunPriority`
    /// for why this is a match on a semantic enum rather than a number the
    /// caller supplies directly.
    fn concurrency_for(&self, priority: RunPriority) -> usize {
        match priority {
            RunPriority::Normal => self.concurrency,
            RunPriority::Low => self.low_priority_concurrency,
        }
    }

    /// FR-FC-26: the requested root must be the configured library root or a
    /// descendant of it. Returns `Ok(())` unconditionally when no library
    /// root is configured.
    ///
    /// Both sides are canonicalized before comparison. That is what makes the
    /// check hold against `<root>/../../etc` (the traversal is resolved away),
    /// against `<root>` vs `<root>/` vs `<root>/.` (all resolve to the same
    /// path), and against a symlinked root (both sides resolve to the link
    /// target). The comparison itself is `Path::starts_with`, which matches
    /// whole path components — a string prefix test would let `/library-evil`
    /// slip past a `/library` bound.
    fn check_root_within_library(&self, requested: &str) -> Result<(), DomainError> {
        let Some(library_root) = self.library_root.as_deref() else {
            return Ok(());
        };
        // A configured root that cannot be resolved is a misconfiguration, not
        // a caller error. Fail the request rather than silently degrading to
        // unconstrained indexing: a security bound that disappears when its
        // configuration is wrong is worse than no bound at all, because the
        // operator believes it is there. The process still starts and every
        // other operation still works — only indexing is refused, and the log
        // names the key to fix.
        let canonical_library_root = match std::fs::canonicalize(library_root) {
            Ok(path) => path,
            Err(err) => {
                tracing::error!(
                    root = %library_root,
                    error = %err,
                    "configured filesystem.root cannot be resolved; refusing to index until it is fixed"
                );
                return Err(DomainError::InvalidInput(LIBRARY_ROOT_UNRESOLVABLE.into()));
            }
        };
        // The requested root's existence was already checked above, so a
        // canonicalization failure here means the path cannot be resolved to
        // something comparable. Fail closed.
        let canonical_requested = std::fs::canonicalize(requested)
            .map_err(|_| DomainError::InvalidInput(OUTSIDE_LIBRARY_ROOT.into()))?;
        if canonical_requested.starts_with(&canonical_library_root) {
            Ok(())
        } else {
            Err(DomainError::InvalidInput(OUTSIDE_LIBRARY_ROOT.into()))
        }
    }

    /// Validate and start — returns a run id without doing any scanning.
    ///
    /// FR-FC-27: the run record opens only after the root is validated, so an
    /// invalid root (AF-01 / AF-06) never leaves a stray record behind.
    /// Unlike `finish`/`fail`, this write's failure is not swallowed after
    /// retrying: if the record cannot be opened at all, the caller must not
    /// receive a run id it can never query.
    pub async fn start(
        &self,
        request: IndexRequest,
        token: &str,
    ) -> Result<IndexStarted, DomainError> {
        self.auth.authenticate(token).await?;
        if !self.fs.path_exists(&request.root).await {
            return Err(DomainError::InvalidInput("root path does not exist".into()));
        }
        self.check_root_within_library(&request.root)?;
        let run_id = Uuid::new_v4();
        let started_at = self.clock.now();
        let concurrency = self.concurrency_for(request.priority) as u32;
        // The scope is recorded here, beside the root, for the reason the
        // root is: both are what the run was told to cover, and a resume
        // that could not read the scope back would walk the very types the
        // owner excluded (FR-FC-33).
        let scope = request.scope.to_wire();
        retry_on_busy(BUSY_ATTEMPTS, || {
            self.runs.start(
                run_id,
                RunKind::Index,
                Some(&request.root),
                started_at,
                concurrency,
                scope.as_deref(),
            )
        })
        .await?;
        Ok(IndexStarted { run_id })
    }

    /// Walk, classify, stat, and persist — no bytes hashed (FR-FC-09). Skips
    /// unsupported extensions and paths already cataloged (AF-03). Completion is logged at `info`.
    ///
    /// Up to `concurrency` entries are in flight at once. The order files are
    /// processed in is therefore unspecified — the outcome counts are not,
    /// since each entry contributes exactly one outcome regardless of when it
    /// finishes. Two entries naming the same path would race the
    /// already-cataloged check (AF-03), but `list_files` cannot produce a path
    /// twice, and the `files.path` unique constraint turns any such duplicate
    /// into that entry's own `failed` rather than a corrupt second record.
    ///
    /// A failure that concerns one specific file — its bytes cannot be read, or
    /// a repository write for it fails — is counted in `failed`, logged at
    /// `warn`, and the walk continues. One locked file must not abandon the
    /// rest of the library. Only a failure to list the root at all aborts.
    ///
    /// `scope` is an argument for the same reason `root` is: both say what
    /// this walk is to cover, and both are handed in by whoever spawned it —
    /// `IndexRequest::scope` for a fresh run, and `RunResumed::scope`, read
    /// back off the run's row, for a resumed one.
    pub async fn execute(
        &self,
        root: &str,
        run_id: Uuid,
        scope: &IndexScope,
    ) -> Result<IndexOutcome, DomainError> {
        let now = self.clock.now();
        // Read from the run's own row rather than carried in as a parameter
        // or held in a field: `IndexHandler` is long-lived (built once at
        // startup) and `execute` is called for both a fresh run and a
        // resumed one, so the field it was built with cannot tell the two
        // apart. This is what makes `given_a_resumed_run_when_executed_...`
        // true — a resumed run reuses the width `start` wrote rather than
        // whatever `self.concurrency` happens to be today. The cost is one
        // extra SELECT per run (not per file), which is negligible next to a
        // walk that takes minutes.
        //
        // Three outcomes, not two: a run with no stored width — one started
        // before run priority existed, or one `execute` is asked to run
        // without ever having called `start` (as several unit tests do) —
        // falls back to the configured `Normal` width with no fuss, mirroring
        // `RunControlHandler::resume`'s own fallback. A *failed* read is
        // different and gets a `warn`: pacing is not the walk's job to
        // guarantee, so a transient store error here must not abort a scan
        // that could perfectly well run at the default width — that would be
        // a correctness regression caused by a performance knob. The default
        // is still the right fallback for either case, but only one of them
        // is silent.
        //
        // Retried on a busy database like every other call this handler makes
        // on the run repository. The fallback above still catches a failure —
        // but a resumed low-priority run degrading to the default width
        // because one `SELECT` met `SQLITE_BUSY` is a real regression, and
        // being the single unretried call on this port is the kind of
        // inconsistency that gets copied into the next one.
        //
        // The same read yields the run's segment, which `record_halt` needs
        // at the bottom of this function — see `CatalogRunRepository::pause`
        // for what a late pause with no segment to match cannot tell apart.
        // `None`, for a row that is absent or unreadable, waives that check
        // rather than inventing a segment for the write to match against.
        let (concurrency, segment) =
            match retry_on_busy(BUSY_ATTEMPTS, || self.runs.get(run_id)).await {
                Ok(Some(run)) => (
                    run.concurrency
                        .map(|c| c.max(1) as usize)
                        .unwrap_or(self.concurrency),
                    Some(run.segment),
                ),
                Ok(None) => (self.concurrency, None),
                Err(err) => {
                    tracing::warn!(
                        %run_id,
                        error = %err,
                        "could not read the run's configured concurrency; falling back to \
                         indexing.concurrency"
                    );
                    (self.concurrency, None)
                }
            };
        // FR-FC-28: the run becomes observable here. A fresh cell reads as
        // `Discovering` with no total, which is exactly where the walk below
        // starts, and the guard closes the run again at every exit — a
        // panic or a task abort included, which no explicit call can cover.
        let run_cell = self.registry.open(run_id);
        let entries = match self.fs.list_files(root).await {
            Ok(entries) => entries,
            Err(err) => {
                // Closed before the terminal write rather than at end of
                // scope: a reader landing between `fail` and the guard's own
                // drop would overlay a live cell on a failed run.
                drop(run_cell);
                // FR-FC-27: the walk could not proceed at all — that, and
                // only that, is a `failed` run.
                let fail_error = err.to_string();
                let failed_at = self.clock.now();
                if let Err(record_err) = retry_on_busy(BUSY_ATTEMPTS, || {
                    self.runs.fail(run_id, &fail_error, failed_at)
                })
                .await
                {
                    // The walk's own error is the one that matters to the
                    // caller — a bookkeeping failure on top of it must not
                    // replace it. The record stays `running` until startup
                    // reconciliation (FR-FC-29) pauses it for resume.
                    tracing::warn!(%run_id, error = %record_err, "could not record run failure");
                }
                return Err(err);
            }
        };
        let scanned = entries.len();
        let cell: &RunCell = &run_cell;
        // Discovery is done: the denominator is known, so both halves of the
        // progress fraction are meaningful from here on. Published before the
        // signal check below, not after, so a run stopped right here still
        // records what discovery counted rather than a NULL total.
        cell.set_total(scanned);
        // Checked exactly once, here: `walkdir`'s collect above is a single
        // blocking call with no interruption point, and discovery is seconds
        // against a walk of minutes. The phase is deliberately *not* advanced
        // to `Processing` first — for a pause, `phase = 'discovering'` is what
        // tells a client the run stopped before the processing loop ever
        // began.
        let signal = cell.signal();
        if signal != RunSignal::None {
            flush_progress(&self.runs, run_id, &cell.snapshot()).await;
            // Closed before the terminal write, as on every other exit.
            drop(run_cell);
            // An all-zero tally, which is the truth: discovery counted
            // `scanned` entries and the loop processed none of them.
            let counts = RunCounts::Index {
                scanned,
                indexed: 0,
                skipped: 0,
                already_cataloged: 0,
                failed: 0,
            };
            record_halt(
                &self.runs,
                run_id,
                signal,
                counts,
                self.clock.now(),
                segment,
            )
            .await;
            tracing::info!(%run_id, scanned, ?signal, "indexing stopped during discovery");
            return Ok(IndexOutcome {
                run_id,
                scanned,
                indexed: 0,
                skipped: 0,
                already_cataloged: 0,
                failed: 0,
            });
        }
        // Flushed immediately rather than waiting out an interval, so a client
        // that reads the row right after the walk sees the phase it is
        // actually in.
        cell.set_phase(RunPhase::Processing);
        flush_progress(&self.runs, run_id, &cell.snapshot()).await;
        // The interval is measured from here, not from the clock read at the
        // top of `execute`: seeded with the pre-discovery time, a walk that
        // itself took longer than the interval would make the very first
        // entry flush again immediately after the one just above.
        let loop_started = self.clock.now();

        let tally = stream::iter(entries)
            .map(|entry| async move {
                match cell.signal() {
                    RunSignal::None => {}
                    // The window drains rather than aborting: entries already
                    // in flight are a stat and a header read each, so this
                    // costs milliseconds. Draining is what lets the tally be
                    // written once, correctly, after the last one lands.
                    RunSignal::Pause | RunSignal::Cancel => return EntryOutcome::Halted,
                }
                let Some(file_type) = classify_by_extension(&entry.name) else {
                    return EntryOutcome::Skipped;
                };
                // Filtered after classification because the type is what is
                // being filtered on, and only the classifier knows it. The
                // outcome is the same `Skipped` an unsupported extension
                // takes: in both cases the run saw the file and chose not to
                // record it, and a second counter would split one fact across
                // two numbers every reader would then have to add up.
                if !scope.includes(file_type) {
                    return EntryOutcome::Skipped;
                }
                let path = entry.path.clone();
                match self.index_entry(entry, file_type, now).await {
                    Ok(outcome) => outcome,
                    Err(err) => {
                        tracing::warn!(
                            %run_id,
                            path = %path,
                            error = %err,
                            "skipping file that could not be indexed"
                        );
                        EntryOutcome::Failed
                    }
                }
            })
            .buffer_unordered(concurrency)
            .fold(
                IndexTally::new(loop_started),
                |mut tally, outcome| async move {
                    match outcome {
                        EntryOutcome::Indexed => tally.indexed += 1,
                        EntryOutcome::Skipped => tally.skipped += 1,
                        EntryOutcome::AlreadyCataloged => tally.already_cataloged += 1,
                        EntryOutcome::Failed => tally.failed += 1,
                        // Counted nowhere, and not advanced past either: this
                        // entry was never processed, and a paused run
                        // reporting `processed == total` would be claiming it
                        // got through files it never opened — which is
                        // exactly the number a resume reads.
                        EntryOutcome::Halted => return tally,
                    }
                    // FR-FC-28: one entry done, whatever it resolved to. A
                    // progress bar that stalled on the skipped and unreadable
                    // files would misreport how far along the run is.
                    cell.advance();
                    // The flush is inline rather than on a timer task of its
                    // own: `execute` is generic over its collaborators, and
                    // spawning would force `Send + 'static` onto every one of
                    // them and leave a task whose lifetime has to be tied back
                    // to this run. The honest cost is not just the clock read
                    // every entry pays — on the entry that crosses the interval
                    // the loop awaits a SQLite write, and `buffer_unordered`
                    // polls none of its in-flight futures meanwhile, so the walk
                    // stalls for that write's latency once every two seconds.
                    // That is still the better trade: one short stall per
                    // interval against `Send + 'static` on ten collaborators and
                    // a task lifetime to keep in step with the run.
                    let now = self.clock.now();
                    if (now - tally.last_flush).num_seconds() >= PROGRESS_FLUSH_SECONDS {
                        flush_progress(&self.runs, run_id, &cell.snapshot()).await;
                        tally.last_flush = now;
                    }
                    tally
                },
            )
            .await;
        let IndexTally {
            indexed,
            skipped,
            already_cataloged,
            failed,
            ..
        } = tally;

        // Whether the walk ran to the end or was stopped partway through.
        // Read once, after the window has drained, so the log line and the
        // row below cannot disagree about what happened.
        //
        // A signal raised after the last entry folded is honoured all the
        // same: no entry was halted, the tally is complete, and yet the run
        // records `paused` at `processed == total` and never writes a `finish`
        // tally. That is intended rather than a gap. Resuming such a run finds
        // nothing left to do, so nothing is lost — but the run does read
        // `paused` until someone resumes or cancels it, which is the honest
        // answer to a pause that arrived before the run had recorded itself.
        let signal = cell.signal();

        // The run is over: publish the final tally before the cell goes away,
        // so a read after this point falls back to the true end state rather
        // than to whatever the last interval flush happened to catch. Then
        // close, ahead of the terminal write below, so no reader can overlay
        // a live cell on a run that has already stopped.
        flush_progress(&self.runs, run_id, &cell.snapshot()).await;
        drop(run_cell);

        tracing::info!(
            %run_id,
            scanned,
            indexed,
            skipped,
            already_cataloged,
            failed,
            // For a halted run these five do not add up: `scanned` counts
            // every entry discovery found, and the difference is the entries
            // that were never processed. See `EntryOutcome::Halted`.
            ?signal,
            "{}",
            match signal {
                RunSignal::None => "indexing complete",
                RunSignal::Pause => "indexing paused",
                RunSignal::Cancel => "indexing cancelled",
            }
        );
        let ended_at = self.clock.now();
        if signal == RunSignal::None {
            // FR-FC-27: the walk finished. Per-file failures are inside the
            // tally and do not make the run failed.
            let counts = RunCounts::Index {
                scanned,
                indexed,
                skipped,
                already_cataloged,
                failed,
            };
            if let Err(err) =
                retry_on_busy(BUSY_ATTEMPTS, || self.runs.finish(run_id, counts, ended_at)).await
            {
                // FR-FC-27: the walk succeeded — only the bookkeeping write
                // failed. Reporting the run as failed would be a lie about
                // work that did happen, and the tally is already in the log
                // line above. The record stays `running` until startup
                // reconciliation (FR-FC-29) pauses it for resume.
                tracing::warn!(%run_id, error = %err, "could not record run completion");
            }
        } else {
            // No `finish`: the walk did not finish, so a `complete` status
            // would misreport a partial pass. The tally still travels —
            // `record_halt` keeps it for a cancel, whose partial counts are
            // final, and drops it for a pause, whose are not.
            let counts = RunCounts::Index {
                scanned,
                indexed,
                skipped,
                already_cataloged,
                failed,
            };
            record_halt(&self.runs, run_id, signal, counts, ended_at, segment).await;
        }
        Ok(IndexOutcome {
            run_id,
            scanned,
            indexed,
            skipped,
            already_cataloged,
            failed,
        })
    }

    /// Index one already-classified entry. `Ok(EntryOutcome::Indexed)` means a
    /// record was created, `Ok(EntryOutcome::AlreadyCataloged)` that the path
    /// was already in the catalog (AF-03), and `Err` that this one file
    /// failed — the caller counts it and moves on. Never returns
    /// `EntryOutcome::Skipped`: that outcome belongs to `execute`'s
    /// classification branch, which never calls this method for an
    /// unsupported extension.
    ///
    /// The `insert_file` write is wrapped in [`retry_on_busy`]: with several
    /// files in flight and a client reading throughout, a loaded machine can
    /// push a writer past SQLite's `busy_timeout` into `SQLITE_BUSY`, which
    /// used to lose that file from the catalog silently but for a `warn`.
    async fn index_entry(
        &self,
        entry: FileEntry,
        file_type: FileType,
        now: DateTime<Utc>,
    ) -> Result<EntryOutcome, DomainError> {
        if self.repo.find_by_path(&entry.path).await?.is_some() {
            return Ok(EntryOutcome::AlreadyCataloged);
        }
        let new_file = NewFile {
            uuid: Uuid::new_v4(),
            path: entry.path.clone(),
            name: entry.name,
            file_type,
            // Not computed here, and deliberately: reading every byte of every
            // file is what made a 418 GB library take tens of minutes. Size
            // and mtime are the change signal now (FR-FC-10). `content_hash`
            // stays `None` unless and until UC-33 edits this file — nothing
            // else writes it (FR-FC-09).
            content_hash: None,
            size_bytes: Some(entry.size_bytes),
            mtime: entry.modified_at,
            indexed_at: now,
        };
        // Only this one write is retried, and only on a transient busy. The
        // directory walk and the stat above are not: they have already
        // succeeded, and re-walking the filesystem to answer database
        // contention would repeat work that is already done. A file that is
        // still busy after the bound falls through exactly as before —
        // counted in `failed`, logged, run continues.
        let file = retry_on_busy(BUSY_ATTEMPTS, || {
            let new_file = new_file.clone();
            async { self.repo.insert_file(new_file).await }
        })
        .await?;

        // Best-effort audio tag prefill (issue #44 pilot). Extraction only
        // ever runs here, at first index — refresh never touches metadata.
        // A parse failure or a write failure here must not fail indexing
        // (it is not counted in `IndexOutcome::failed`).
        if file_type == FileType::Audio {
            if let Some(metadata) = self
                .audio_tags
                .read(&entry.path)
                .await
                .and_then(|tags| tags.into_subtype_metadata())
            {
                if let Err(err) = self.repo.update_metadata(file.uuid, &metadata).await {
                    tracing::warn!(
                        path = %entry.path,
                        error = %err,
                        "indexed but failed to write extracted audio tags"
                    );
                }
            }
        }

        // Best-effort image EXIF prefill (issue #44 image slice). Two
        // independent writes: dimensions (outside SubtypeMetadata, via
        // set_image_dimensions) and title (via the shared update_metadata,
        // same as audio). Neither write's failure blocks the other or fails
        // indexing.
        if file_type == FileType::Image {
            if let Some(tags) = self.image_tags.read(&entry.path).await {
                if let (Some(width), Some(height)) = (tags.width, tags.height) {
                    if let Err(err) = self
                        .repo
                        .set_image_dimensions(file.uuid, width, height)
                        .await
                    {
                        tracing::warn!(
                            path = %entry.path,
                            error = %err,
                            "indexed but failed to write extracted image dimensions"
                        );
                    }
                }
                if let Some(title) = tags.title {
                    // caption: None is safe only because extraction runs
                    // exactly once, at first index, before an owner could
                    // have set one via UC-04 — update_metadata is a full
                    // replace, so reusing this pattern anywhere caption
                    // might already be set would silently wipe it.
                    let metadata = crate::catalog::model::SubtypeMetadata::Image {
                        title: Some(title),
                        caption: None,
                    };
                    if let Err(err) = self.repo.update_metadata(file.uuid, &metadata).await {
                        tracing::warn!(
                            path = %entry.path,
                            error = %err,
                            "indexed but failed to write extracted image title"
                        );
                    }
                }
            }
        }

        // Best-effort document metadata prefill (issue #44 document
        // slice). Two independent writes: page count (outside
        // SubtypeMetadata, via set_document_page_count — PDF only, EPUB
        // never sets it) and title/author/year/format_kind (via the
        // shared update_metadata). Neither write's failure blocks the
        // other or fails indexing.
        if file_type == FileType::Document {
            if let Some(tags) = self.document_tags.read(&entry.path).await {
                if let Some(page_count) = tags.page_count {
                    if let Err(err) = self
                        .repo
                        .set_document_page_count(file.uuid, page_count)
                        .await
                    {
                        tracing::warn!(
                            path = %entry.path,
                            error = %err,
                            "indexed but failed to write extracted document page count"
                        );
                    }
                }
                if tags.title.is_some()
                    || tags.author.is_some()
                    || tags.year.is_some()
                    || tags.format_kind.is_some()
                {
                    let metadata = crate::catalog::model::SubtypeMetadata::Document {
                        title: tags.title,
                        author: tags.author,
                        year: tags.year,
                        format_kind: tags.format_kind,
                    };
                    if let Err(err) = self.repo.update_metadata(file.uuid, &metadata).await {
                        tracing::warn!(
                            path = %entry.path,
                            error = %err,
                            "indexed but failed to write extracted document metadata"
                        );
                    }
                }
            }
        }

        // Best-effort video metadata prefill (issue #44 video slice). Two
        // independent writes: duration (outside SubtypeMetadata, via
        // set_video_duration) and title/year/resolution (via the shared
        // update_metadata, media_kind always None — it is not inferable
        // from the file). Neither write's failure blocks the other or
        // fails indexing.
        if file_type == FileType::Video {
            if let Some(tags) = self.video_tags.read(&entry.path).await {
                if let Some(crate::catalog::video_tags::VideoDuration(duration_seconds)) =
                    tags.duration_seconds
                {
                    if let Err(err) = self
                        .repo
                        .set_video_duration(file.uuid, duration_seconds)
                        .await
                    {
                        tracing::warn!(
                            path = %entry.path,
                            error = %err,
                            "indexed but failed to write extracted video duration"
                        );
                    }
                }
                if tags.title.is_some() || tags.year.is_some() || tags.resolution.is_some() {
                    let metadata = crate::catalog::model::SubtypeMetadata::Video {
                        title: tags.title,
                        year: tags.year,
                        resolution: tags.resolution,
                        media_kind: None,
                    };
                    if let Err(err) = self.repo.update_metadata(file.uuid, &metadata).await {
                        tracing::warn!(
                            path = %entry.path,
                            error = %err,
                            "indexed but failed to write extracted video metadata"
                        );
                    }
                }
            }
        }

        // Best-effort comic metadata prefill (issue #44 comic slice). Two
        // independent writes: page count (outside SubtypeMetadata, via
        // set_comic_page_count — always present once the archive opens)
        // and title/series/issue_number (via the shared update_metadata).
        // Neither write's failure blocks the other or fails indexing.
        if file_type == FileType::Comic {
            if let Some(tags) = self.comic_tags.read(&entry.path).await {
                if let Some(page_count) = tags.page_count {
                    if let Err(err) = self.repo.set_comic_page_count(file.uuid, page_count).await {
                        tracing::warn!(
                            path = %entry.path,
                            error = %err,
                            "indexed but failed to write extracted comic page count"
                        );
                    }
                }
                if tags.title.is_some() || tags.series.is_some() || tags.issue_number.is_some() {
                    let metadata = crate::catalog::model::SubtypeMetadata::Comic {
                        title: tags.title,
                        series: tags.series,
                        issue_number: tags.issue_number,
                    };
                    if let Err(err) = self.repo.update_metadata(file.uuid, &metadata).await {
                        tracing::warn!(
                            path = %entry.path,
                            error = %err,
                            "indexed but failed to write extracted comic metadata"
                        );
                    }
                }
            }
        }
        Ok(EntryOutcome::Indexed)
    }
}
