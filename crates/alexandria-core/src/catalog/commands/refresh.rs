use chrono::{DateTime, Utc};
use futures_util::stream::{self, StreamExt};
use serde::Serialize;
use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::audio_tags::AudioMetadataReader;
use crate::catalog::clock::Clock;
use crate::catalog::comic_tags::ComicMetadataReader;
use crate::catalog::commands::{flush_progress, record_halt, PROGRESS_FLUSH_SECONDS};
use crate::catalog::document_tags::DocumentMetadataReader;
use crate::catalog::extraction::{MetadataExtractor, MetadataWrite};
use crate::catalog::fs::Filesystem;
use crate::catalog::image_tags::ImageMetadataReader;
use crate::catalog::model::File;
use crate::catalog::model::METADATA_VERSION;
use crate::catalog::repos::CatalogRepository;
use crate::catalog::run_registry::{RunCell, RunPhase, RunRegistry, RunSignal};
use crate::catalog::runs::{CatalogRunRepository, RunCounts, RunKind, RunPriority};
use crate::catalog::video_tags::VideoMetadataReader;
use crate::errors::DomainError;
use crate::retry::{retry_on_busy, BUSY_ATTEMPTS};

#[derive(Debug, Clone, Serialize)]
pub struct RefreshStarted {
    #[serde(rename = "runId")]
    pub run_id: Uuid,
}

#[derive(Debug, Clone, Serialize)]
pub struct RefreshOutcome {
    #[serde(rename = "runId")]
    pub run_id: Uuid,
    /// Records whose stat changed (or that returned to disk while marked
    /// missing) and were refreshed. Stat, not hash — Task 4 replaced the
    /// SHA-256 comparison with a size/mtime one (FR-FC-10).
    pub refreshed: usize,
    /// Cataloged paths whose on-disk file is gone (UC-02 AF-01 / FR-FC-11).
    pub marked_missing: usize,
    /// Present and unchanged since the last index — no write performed.
    pub unchanged: usize,
    /// Rows whose metadata an older extraction wrote, re-read from the file
    /// and filled in (UC-02). Counted apart from `refreshed`, which is about
    /// the file having changed on disk: these files did not change at all,
    /// the catalog's reading of them did.
    #[serde(rename = "metadataFilled")]
    pub metadata_filled: usize,
    /// Cataloged paths that could not be processed because an operation against
    /// that one file failed (unreadable bytes, or a repository write error).
    /// The run continues past them; each is logged at `warn`.
    pub failed: usize,
}

/// Re-index and refresh the catalog (UC-02).
///
/// `start` authenticates the caller and returns a fresh run id immediately;
/// `execute` iterates every cataloged path (no tree walk — discovery of *new*
/// files is UC-01's job), stats each present file, and:
///   * refreshes size/mtime + `indexed_at` (clearing `content_hash` and
///     `missing_at`) when the stat changed or the file returned to disk after
///     being marked missing (FR-FC-10), and
///   * marks `missing_at` (leaving `state` untouched — soft-delete is UC-06)
///     when the on-disk file is gone (FR-FC-11 / AF-01).
///
/// Task 4 replaced the SHA-256 comparison this used to make with a single
/// `stat` call: cost used to scale with the library's total *size* (every
/// byte of every file, every run); now it scales with its file *count* (one
/// syscall per file). A cataloged file's `content_hash` is `None` unless
/// UC-33 has edited it — refresh never restores it, since it never reads
/// bytes to compute one.
///
/// Like `IndexHandler`, `execute` processes up to `concurrency` cataloged
/// paths at a time, where `concurrency` is the width the run's own
/// `RunPriority` resolved to at `start`, or at the `resume` that last re-paced
/// it (`RunControlHandler::resume`) — `indexing.concurrency` /
/// `indexing.low_priority_concurrency`, the same two settings `IndexHandler`
/// uses (a re-index is the same one-stat-per-file workload as an index, so
/// splitting the knobs per command would only invite them to disagree), read
/// back from the run's stored `concurrency` column exactly as
/// `IndexHandler::execute` does.
///
/// Generic over collaborators so the decision logic is unit-tested against
/// trait fakes with no real DB / filesystem / auth service (Testing Spec §6.2).
pub struct RefreshHandler<A, R, F, C, RR, M, N, O, P, Q> {
    auth: A,
    repo: R,
    fs: F,
    clock: C,
    /// The same five readers `IndexHandler` holds, for the same reason and
    /// through the same collaborator: a row written by an older extraction
    /// is re-read here, and it must be read exactly as a first index would
    /// read it (`MetadataExtractor`).
    extractor: MetadataExtractor<M, N, O, P, Q>,
    concurrency: usize,
    /// The width a `RunPriority::Low` run refreshes at
    /// (`indexing.low_priority_concurrency`). See `concurrency` for the
    /// `Normal` counterpart and the zero clamp both share.
    low_priority_concurrency: usize,
    runs: RR,
    /// Where `execute` publishes this run's live progress (FR-FC-28). Shared
    /// with `GetRunStatusHandler`, which reads it back.
    registry: RunRegistry,
}

impl<A, R, F, C, RR, M, N, O, P, Q> RefreshHandler<A, R, F, C, RR, M, N, O, P, Q>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
    C: Clock,
    RR: CatalogRunRepository,
    M: AudioMetadataReader,
    N: ImageMetadataReader,
    O: DocumentMetadataReader,
    P: VideoMetadataReader,
    Q: ComicMetadataReader,
{
    /// `concurrency` is how many cataloged paths `execute` refreshes at a
    /// time for a `RunPriority::Normal` run; `low_priority_concurrency` is
    /// the same for `RunPriority::Low`. Zero is clamped to 1 for either, as
    /// in `IndexHandler::new`.
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
        runs: RR,
        registry: RunRegistry,
    ) -> Self {
        Self {
            auth,
            repo,
            fs,
            clock,
            extractor: MetadataExtractor::new(
                audio_tags,
                image_tags,
                document_tags,
                video_tags,
                comic_tags,
            ),
            concurrency: concurrency.max(1) as usize,
            low_priority_concurrency: low_priority_concurrency.max(1) as usize,
            runs,
            registry,
        }
    }

    /// The width a run at `priority` should be walked at. See
    /// `IndexHandler::concurrency_for`, which this mirrors.
    fn concurrency_for(&self, priority: RunPriority) -> usize {
        match priority {
            RunPriority::Normal => self.concurrency,
            RunPriority::Low => self.low_priority_concurrency,
        }
    }

    /// Authenticate and return a run id. No input to validate (re-index
    /// touches every cataloged path), so the only failure is AF-02.
    ///
    /// FR-FC-27: a started run is always a recorded run — the record opens
    /// here, where the id is minted, so no caller can start one without it.
    /// Unlike `finish`/`fail`, this write's failure is not swallowed after
    /// retrying: if the record cannot be opened at all, the caller must not
    /// receive a run id it can never query.
    pub async fn start(
        &self,
        priority: RunPriority,
        token: &str,
    ) -> Result<RefreshStarted, DomainError> {
        self.auth.authenticate(token).await?;
        let run_id = Uuid::new_v4();
        let started_at = self.clock.now();
        let concurrency = self.concurrency_for(priority) as u32;
        retry_on_busy(BUSY_ATTEMPTS, || {
            // No root and no scope: a refresh discovers through the catalog
            // rather than through a walk (FR-FC-28), so it has neither a tree
            // to be pointed at nor types to filter out of one.
            self.runs.start(
                run_id,
                RunKind::Refresh,
                None,
                started_at,
                concurrency,
                None,
            )
        })
        .await?;
        Ok(RefreshStarted { run_id })
    }

    /// Walk every cataloged path and refresh / mark missing.
    ///
    /// Up to `concurrency` paths are in flight at once, so the order they are
    /// visited in is unspecified. Each path's outcome depends only on that
    /// path's own row and its own bytes, so the tallies do not depend on the
    /// order — every row contributes exactly one outcome.
    ///
    /// A failure that concerns one specific file — its bytes cannot be read, or
    /// a repository write for it fails — is counted in `failed`, logged at
    /// `warn`, and the walk continues. One locked file must not abandon the
    /// rest of the catalog. Only a failure to list the catalog at all aborts.
    pub async fn execute(&self, run_id: Uuid) -> Result<RefreshOutcome, DomainError> {
        let now = self.clock.now();
        // Read from the run's own row, not a field — see
        // `IndexHandler::execute`'s comment on the same read, which explains
        // why (a resumed run reusing the width it was started with), what it
        // costs (one extra SELECT per run), and why a failed read (unlike an
        // absent row) is worth a `warn` — it must still fall back rather than
        // abort a walk that could perfectly well run at the default width.
        // The same comment covers why the read is retried on a busy database,
        // and why it also yields the run's segment for `record_halt` below.
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
        // FR-FC-28: a refresh's discovery is `list_all` rather than a
        // filesystem walk, but it is the same shape — a phase with no
        // denominator, then a phase with one — so it gets the same treatment.
        let run_cell = self.registry.open(run_id);
        let files = match self.repo.list_all().await {
            Ok(files) => files,
            Err(err) => {
                // Closed before the terminal write rather than at end of
                // scope, as in `IndexHandler::execute`.
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

        let cataloged = files.len();
        let cell: &RunCell = &run_cell;
        // Discovery is done: the denominator is known. Published before the
        // signal check, so a run stopped right here still records what the
        // listing counted — see `IndexHandler::execute`, which does all of
        // this for the same reasons.
        cell.set_total(cataloged);
        let signal = cell.signal();
        if signal != RunSignal::None {
            flush_progress(&self.runs, run_id, &cell.snapshot()).await;
            // Closed before the terminal write, as on every other exit.
            drop(run_cell);
            // An all-zero tally: the listing found `cataloged` paths and the
            // loop processed none of them.
            let counts = RunCounts::Refresh {
                refreshed: 0,
                marked_missing: 0,
                unchanged: 0,
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
            // `cataloged` is logged for the same reason the index walk logs
            // `scanned`: without a quantity an operator cannot tell how large
            // the run that stopped was.
            tracing::info!(%run_id, cataloged, ?signal, "re-index stopped during discovery");
            return Ok(RefreshOutcome {
                run_id,
                refreshed: 0,
                marked_missing: 0,
                unchanged: 0,
                metadata_filled: 0,
                failed: 0,
            });
        }

        // Flushed immediately rather than waiting out an interval, so a client
        // that reads the row right after the listing sees the phase it is
        // actually in.
        cell.set_phase(RunPhase::Processing);
        flush_progress(&self.runs, run_id, &cell.snapshot()).await;
        // The interval runs from here, not from the clock read at the top of
        // `execute` — see `IndexHandler::execute`.
        let loop_started = self.clock.now();

        let tally = stream::iter(files)
            .map(|file| async move {
                match cell.signal() {
                    RunSignal::None => {}
                    // The window drains rather than aborting — a stat each,
                    // milliseconds. See `IndexHandler::execute`.
                    RunSignal::Pause | RunSignal::Cancel => return PathOutcome::Halted,
                }
                match self.refresh_one(&file, now).await {
                    Ok(outcome) => outcome,
                    Err(err) => {
                        tracing::warn!(
                            %run_id,
                            path = %file.path,
                            error = %err,
                            "skipping cataloged path that could not be refreshed"
                        );
                        // Recorded by path as well as counted (FR-FC-42),
                        // for the same reason the index walk does it: a
                        // record that could not be re-checked carries
                        // whatever state the last successful run left, and
                        // the owner cannot act on a number alone.
                        if let Err(note) = self
                            .runs
                            .record_failure(run_id, &file.path, &err.to_string(), now)
                            .await
                        {
                            tracing::warn!(
                                %run_id,
                                path = %file.path,
                                error = %note,
                                "could not record which file failed"
                            );
                        }
                        PathOutcome::Failed
                    }
                }
            })
            .buffer_unordered(concurrency)
            .fold(
                RefreshTally::new(loop_started),
                |mut tally, outcome| async move {
                    match outcome {
                        PathOutcome::Refreshed => tally.refreshed += 1,
                        PathOutcome::MarkedMissing => tally.marked_missing += 1,
                        PathOutcome::Unchanged => tally.unchanged += 1,
                        PathOutcome::MetadataFilled => tally.metadata_filled += 1,
                        PathOutcome::Failed => tally.failed += 1,
                        // Counted nowhere and not advanced past: this path
                        // was never processed. See `EntryOutcome::Halted`.
                        PathOutcome::Halted => return tally,
                    }
                    // FR-FC-28: one cataloged path done, whatever it resolved to.
                    cell.advance();
                    // Inline rather than on a spawned timer, for the reasons
                    // `IndexHandler::execute` gives — including the one that
                    // costs something: the entry that crosses the interval
                    // awaits a SQLite write while `buffer_unordered` polls none
                    // of its in-flight futures, so the pass stalls for that
                    // write once every two seconds.
                    let now = self.clock.now();
                    if (now - tally.last_flush).num_seconds() >= PROGRESS_FLUSH_SECONDS {
                        flush_progress(&self.runs, run_id, &cell.snapshot()).await;
                        tally.last_flush = now;
                    }
                    tally
                },
            )
            .await;
        let RefreshTally {
            refreshed,
            marked_missing,
            unchanged,
            metadata_filled,
            failed,
            ..
        } = tally;

        // Whether the pass ran to the end or was stopped partway through.
        // Read once, after the window has drained — see
        // `IndexHandler::execute`, including what happens to a signal that
        // arrives after the last path has folded.
        let signal = cell.signal();

        // The run is over: publish the final tally before the cell goes away,
        // then close ahead of the terminal write below so no reader can
        // overlay a live cell on a run that has already stopped.
        flush_progress(&self.runs, run_id, &cell.snapshot()).await;
        drop(run_cell);

        let outcome = RefreshOutcome {
            run_id,
            refreshed,
            marked_missing,
            unchanged,
            metadata_filled,
            failed,
        };
        tracing::info!(
            %run_id,
            refreshed = outcome.refreshed,
            marked_missing = outcome.marked_missing,
            unchanged = outcome.unchanged,
            failed = outcome.failed,
            // For a halted run these do not account for every cataloged path:
            // the difference is the paths that were never processed. See
            // `PathOutcome::Halted`.
            ?signal,
            "{}",
            match signal {
                RunSignal::None => "re-index complete",
                RunSignal::Pause => "re-index paused",
                RunSignal::Cancel => "re-index cancelled",
            }
        );
        let ended_at = self.clock.now();
        if signal == RunSignal::None {
            // FR-FC-27: the walk finished. Per-file failures are inside the
            // tally and do not make the run failed.
            let counts = RunCounts::Refresh {
                refreshed,
                marked_missing,
                unchanged,
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
            // No `finish`: the pass did not finish, so a `complete` status
            // would misreport it. The tally still travels — kept for a cancel,
            // dropped for a pause. See `record_halt`.
            let counts = RunCounts::Refresh {
                refreshed,
                marked_missing,
                unchanged,
                failed,
            };
            record_halt(&self.runs, run_id, signal, counts, ended_at, segment).await;
        }
        Ok(outcome)
    }

    /// Refresh one cataloged path. `Err` means this one file failed — the
    /// caller counts it and moves on to the rest of the catalog.
    ///
    /// Both writes are wrapped in [`retry_on_busy`], for exactly the reason
    /// UC-01's `insert_file` is: this walk runs `concurrency` writers against
    /// SQLite's single writer while a client reads throughout, and a writer
    /// that waits out its whole `busy_timeout` is answered `SQLITE_BUSY`. Left
    /// unretried, that transient contention becomes a `failed` count — a
    /// re-index silently leaving a stale size/mtime or an unmarked missing file
    /// behind, which is worse here than at first index, since nothing else
    /// will revisit that row until the next run.
    async fn refresh_one(
        &self,
        file: &File,
        now: DateTime<Utc>,
    ) -> Result<PathOutcome, DomainError> {
        let Some(stat) = self.fs.stat(&file.path).await? else {
            // UC-02 AF-01 / FR-FC-11: the on-disk file is gone.
            return if file.missing_at.is_none() {
                retry_on_busy(BUSY_ATTEMPTS, || self.repo.mark_missing(&file.path, now)).await?;
                Ok(PathOutcome::MarkedMissing)
            } else {
                // Already marked missing and still gone — leave as-is.
                Ok(PathOutcome::Unchanged)
            };
        };

        // FR-FC-10: size and mtime are the change signal — no bytes read. A
        // file that returned to disk while marked missing is refreshed even
        // when its stats match, because `missing_at` has to be cleared.
        let unchanged = file.size_bytes == Some(stat.size_bytes)
            && file.mtime == stat.modified_at
            && file.missing_at.is_none();
        if unchanged {
            // The file has not changed, but the catalog's reading of it may
            // be behind: extraction has only ever run at first index, so a
            // row written before the extractor learned a field carries a gap
            // nothing else would ever close. This is where it closes —
            // once per row, because the row is stamped afterwards.
            return match self.fill_metadata(file, WhenBehind).await? {
                Filled::Yes => Ok(PathOutcome::MetadataFilled),
                Filled::No => Ok(PathOutcome::Unchanged),
            };
        }

        retry_on_busy(BUSY_ATTEMPTS, || {
            self.repo
                .refresh_stat(&file.path, stat.size_bytes, stat.modified_at, now)
        })
        .await?;

        // The bytes moved on, so anything measured or fetched from the old
        // ones is now describing a recording this file no longer holds. Both
        // are re-derived on demand and neither is the owner's own work, so
        // dropping them costs a re-measure the next time the track is played
        // and buys back the guarantee that what is drawn is what is heard.
        //
        // Before the tags are re-read rather than after: `fill_metadata` can
        // fail on a file the extractor cannot open, and a stale envelope must
        // not survive on the strength of that.
        retry_on_busy(BUSY_ATTEMPTS, || {
            self.repo.forget_derived_content(file.uuid)
        })
        .await?;

        // A changed file is read whatever its stamp says: its bytes are new,
        // so its tags may be new too, and the stamp only records which
        // extraction last read them — not which bytes it read. Filling still
        // only ever adds, so a retagged file gains the fields it was missing
        // and keeps everything it already had, the owner's edits included.
        //
        // Counted as `Refreshed` all the same: what matters about this path
        // is that the file itself moved on.
        self.fill_metadata(file, Always).await?;

        Ok(PathOutcome::Refreshed)
    }

    /// Re-reads an unchanged file's own metadata when an older extraction
    /// wrote its row, and fills what the catalog is missing (UC-02).
    ///
    /// The gap this closes is a real library's: `album_artist` arrived in
    /// migration 15, every row indexed before it holds NULL there, and
    /// nothing revisited a row once it was written — so an artists list
    /// grouped by the record's own artist fell back to each track's
    /// performer, and a record with guests on it appeared once per guest.
    ///
    /// Three things keep this cheap and safe. It reads a file only while the
    /// row is behind [`METADATA_VERSION`], so a library pays for it once and
    /// never again. It fills rather than replaces
    /// ([`CatalogRepository::fill_missing_metadata`]), so a title the owner
    /// corrected by hand is not overwritten by whatever the tags say. And it
    /// stamps the row whatever the reading gave up, so a file nothing can be
    /// read from is opened once rather than on every pass.
    async fn fill_metadata(&self, file: &File, read: ReadTags) -> Result<Filled, DomainError> {
        if read == WhenBehind && file.metadata_version >= METADATA_VERSION {
            return Ok(Filled::No);
        }

        let filled = self
            .extractor
            .extract_into(
                &self.repo,
                file.uuid,
                &file.path,
                file.file_type,
                MetadataWrite::FillGaps,
            )
            .await;

        // Stamped whatever came back, including nothing.
        //
        // The stamp says which extraction has been *applied* to the row, not
        // which one succeeded, and a file that gave up nothing has had this
        // one applied as fully as it ever will be. Stamping only a successful
        // read looked more careful and was not: a text file named `.flac`, a
        // truncated download, an encrypted document — every one of them would
        // be opened again on every pass for the life of the library, and the
        // retry that bought would only ever pay off when the extractor
        // learned something new, which bumps `METADATA_VERSION` and revisits
        // every row anyway.
        retry_on_busy(BUSY_ATTEMPTS, || {
            self.repo.set_metadata_version(file.uuid, METADATA_VERSION)
        })
        .await?;

        Ok(if filled { Filled::Yes } else { Filled::No })
    }
}

/// When [`RefreshHandler::fill_metadata`] should open the file at all.
///
/// The stamp records which *extraction* last read a row, not which bytes it
/// read — so it answers "is this row behind?" and says nothing about a file
/// whose contents have since changed. The two callers want different things
/// from it, and conflating them is what left a retagged file unread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadTags {
    /// Only while the row is behind the current extraction. The unchanged
    /// path: reading every file on every pass is what the stat comparison
    /// exists to avoid.
    WhenBehind,
    /// Regardless of the stamp. The changed path: new bytes, possibly new
    /// tags.
    Always,
}

use ReadTags::{Always, WhenBehind};

/// Whether a path's metadata was actually re-read and written.
///
/// Its own two-value type rather than a `bool`, because the caller does two
/// different things with the answer: on an unchanged path it decides the
/// path's outcome, and on a changed one it decides nothing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Filled {
    Yes,
    No,
}

/// What a single cataloged path resolved to during a refresh pass.
/// `Failed` is produced by `execute` after it logs the path's error, so the
/// concurrent walk can tally outcomes without sharing a counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathOutcome {
    Refreshed,
    MarkedMissing,
    Unchanged,
    /// Present, unchanged on disk, and re-read because an older extraction
    /// wrote its metadata.
    MetadataFilled,
    Failed,
    /// The run was paused or cancelled before this path did any work. The
    /// counterpart of `EntryOutcome::Halted` — it contributes to no counter
    /// and does not advance `processed`, for the reasons documented there.
    Halted,
}

/// What `execute`'s processing loop carries from one path to the next: the
/// outcome tally, and when it last flushed progress. The counterpart of
/// `IndexHandler`'s `IndexTally`, and `last_flush` rides in the accumulator
/// for the same reason — the spawned `execute` future has to be `Send`.
struct RefreshTally {
    refreshed: usize,
    marked_missing: usize,
    unchanged: usize,
    metadata_filled: usize,
    failed: usize,
    last_flush: DateTime<Utc>,
}

impl RefreshTally {
    /// `started` is when the processing loop began — after the listing and
    /// after the unconditional flush that ends it — so the first interval
    /// flush lands one interval into the loop rather than on its first path.
    fn new(started: DateTime<Utc>) -> Self {
        Self {
            refreshed: 0,
            marked_missing: 0,
            unchanged: 0,
            metadata_filled: 0,
            failed: 0,
            last_flush: started,
        }
    }
}
