//! The indexing coordinator.
//!
//! Per source: offline check → scan → diff against the reusable baseline
//! (empty for a full rebuild) → begin a build → carry unchanged files
//! forward → process new/changed/moved files → publish atomically, or abort
//! and leave the previously published build visible.
//!
//! Concurrency is bounded everywhere: `workers` prepare threads pull from a
//! `sync_channel` of capacity `2 * workers`, results flow back through a
//! channel of the same bound, and exactly one writer (the calling thread)
//! applies them to SQLite in one short transaction per file. Processors
//! never run inside a transaction. A panicking processor fails only its
//! file. A failing source never stops later sources ([`index_all`]).
//!
//! A full build commits in segments ([`Options::segment_files`] files or
//! [`SEGMENT_BYTES`]) while it stays invisible, releasing the write lock
//! and checkpointing the WAL between segments. A run that dies part-way
//! resumes it: files already committed to the pending build with an
//! unchanged stat are not processed again. Full-build finalizers (model
//! inference) run after the last segment, with no transaction open.
//! Incremental passes keep a single transaction, so they stay atomic.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use docmonk_config::DocMonkConfig;
use docmonk_core::errors::{DocMonkError, ErrorKind};
use docmonk_core::ids::record;
use docmonk_core::models::FileKind;
use docmonk_core::paths::{project_id_for_canonical, Home};
use docmonk_storage::control::{plan_for, ControlPlane, IndexVersions, RebuildPlan, SourceRecord};
use docmonk_storage::knowledge::{FileKnowledge, FileRow, ProjectStore};
use docmonk_storage::StorageLayout;
use serde::Serialize;

use crate::diff::{diff, hash_candidates, Change, Decision, DiffCounts};
use crate::fingerprint::hash_file;
use crate::ignore::IgnoreMatcher;
use crate::lock::RunLock;
use crate::retry;
use crate::scan::{check_root_accessible, scan, scan_targets, ScanOptions};

/// What a processor sees for one file.
#[derive(Debug, Clone)]
pub struct PrepareInput {
    pub source_id: String,
    pub file_id: String,
    pub rel_path: String,
    pub path: PathBuf,
    pub kind: FileKind,
    pub content_hash: Option<String>,
}

/// A processor error with this code records the file as `skipped_limit`
/// (a configured resource limit, not a failure).
pub const SKIPPED_LIMIT: &str = "skipped_limit";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessError {
    pub code: String,
    pub message: String,
    /// Transient errors are retried with backoff; others fail permanently.
    pub transient: bool,
}

/// Extracts knowledge from one file. Must be pure with respect to storage:
/// it runs on worker threads, outside any transaction.
pub trait Processor: Send + Sync {
    fn prepare(&self, input: &PrepareInput) -> Result<FileKnowledge, ProcessError>;
}

/// Records the file only. Code and document processors replace it
/// where they apply.
pub struct RawProcessor;

impl Processor for RawProcessor {
    fn prepare(&self, input: &PrepareInput) -> Result<FileKnowledge, ProcessError> {
        std::fs::metadata(&input.path).map_err(|e| ProcessError {
            code: "io_error".into(),
            message: e.to_string(),
            transient: true,
        })?;
        Ok(FileKnowledge::default())
    }
}

/// Whole-build work that runs on the writer after every file of a build is
/// written and before it is published (e.g. embeddings). Relationship
/// extraction, resolution and linking are never finalizers: they run after
/// publication in the separate graph stage. An error aborts the build; the
/// previous build stays visible.
pub trait BuildFinalizer: Send + Sync {
    /// `touched` lists every file id written in this build (all files for
    /// a full rebuild; changed/new/moved files for an incremental pass).
    fn finalize(
        &self,
        store: &mut ProjectStore,
        build_id: &str,
        touched: &[String],
    ) -> Result<FinalizeReport, ProcessError>;

    /// Runs on a warm pass (nothing to rebuild) against the active build,
    /// so derived state that is missing without a file change (for
    /// example vectors lost in a crash, or a model installed later) can
    /// be repaired. Its report is summed into the pass result like
    /// [`finalize`](Self::finalize)'s. Default: nothing.
    fn on_warm_pass(
        &self,
        _store: &mut ProjectStore,
        _build_id: &str,
    ) -> Result<FinalizeReport, ProcessError> {
        Ok(FinalizeReport::default())
    }
}

/// What a finalizer did, summed into the pass result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct FinalizeReport {
    /// Automatic knowledge links inserted.
    pub linked: usize,
    /// Vectors written.
    pub embedded: usize,
}

/// Processors per file kind plus the versions they produce.
pub struct Registry {
    pub code: Arc<dyn Processor>,
    pub document: Arc<dyn Processor>,
    pub unknown: Arc<dyn Processor>,
    pub finalizers: Vec<Arc<dyn BuildFinalizer>>,
    pub versions: IndexVersions,
}

impl Registry {
    pub fn raw() -> Self {
        let raw: Arc<dyn Processor> = Arc::new(RawProcessor);
        Self {
            code: raw.clone(),
            document: raw.clone(),
            unknown: raw,
            finalizers: Vec::new(),
            versions: IndexVersions {
                parser_version: "raw-1".into(),
                chunker_version: "none".into(),
                converter_version: "none".into(),
                embedding_model_id: None,
                embedding_text_version: None,
            },
        }
    }

    fn for_kind(&self, kind: FileKind) -> &dyn Processor {
        match kind {
            FileKind::Code => self.code.as_ref(),
            FileKind::Document => self.document.as_ref(),
            FileKind::Unknown => self.unknown.as_ref(),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ProgressEvent {
    SourceStarted {
        source_id: String,
        position: usize,
        total: usize,
    },
    Stage {
        source_id: String,
        stage: &'static str,
    },
    Scanned {
        source_id: String,
        files: usize,
        to_process: usize,
    },
    FileDone {
        source_id: String,
        done: usize,
        total: usize,
        outcome: FileOutcome,
    },
    /// Liveness from a long stage without per-file completions.
    Heartbeat,
    SourceFinished {
        source_id: String,
        ok: bool,
    },
}

/// What happened to one processed file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileOutcome {
    Indexed,
    Failed,
    Retry,
    Skipped,
}

pub trait Progress {
    fn event(&mut self, event: &ProgressEvent);
}

pub struct NoProgress;
impl Progress for NoProgress {
    fn event(&mut self, _: &ProgressEvent) {}
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct StageTimings {
    pub scan_seconds: f64,
    pub classify_seconds: f64,
    pub hash_calls: usize,
    pub process_seconds: f64,
    pub publish_seconds: f64,
}

/// Concurrency observed in one pass's processing pipeline.
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub struct PipelineStats {
    /// Worker threads.
    pub workers: usize,
    /// Capacity of each bounded channel (work and results).
    pub queue_capacity: usize,
    /// Most files in flight at once (handed to the work queue, result not
    /// yet taken by the writer). Bounded by `2 * queue_capacity + workers +
    /// 2`: both channels full, every worker holding one file, the feeder
    /// waiting with one more, and one result between the channel and the
    /// writer's count.
    pub in_flight_high_water: usize,
    /// Most workers processing a file at the same time.
    pub active_workers_high_water: usize,
    /// Files that went through the pipeline.
    pub items: usize,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct SourceResult {
    pub source_id: String,
    pub offline: Option<String>,
    pub plan: String,
    pub counts: DiffCounts,
    pub indexed: usize,
    pub retrying: usize,
    pub failed: usize,
    pub skipped_limit: usize,
    pub carried_forward: usize,
    pub build_id: Option<String>,
    pub published: bool,
    pub scan_errors: Vec<String>,
    /// Only the named paths were examined (a watcher-triggered pass).
    pub targeted: bool,
    pub linked: usize,
    pub embedded: usize,
    pub attachments: docmonk_storage::knowledge::AttachmentStats,
    /// The source was offline before this pass and is reachable again.
    pub became_online: bool,
    pub timings: StageTimings,
    pub pipeline: PipelineStats,
    /// A build an earlier run left unpublished, discarded by this pass.
    pub recovered_build: Option<String>,
    /// An interrupted full build this pass resumed instead of restarting.
    pub resumed_build: Option<String>,
    /// Segments a full build committed before its last one.
    pub segments: usize,
    /// Server mode: what the publication to the server did (copy-forward
    /// and written record counts, timings).
    pub server_publish: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub workers: usize,
    pub max_file_size_bytes: i64,
    pub follow_symlinks: bool,
    pub cache_size_mb: i64,
    pub lock_timeout: Duration,
    /// Fault injection forwarded to the scanner (tests/drills only).
    pub inject_unreadable: Vec<PathBuf>,
    /// Files a full build commits per segment.
    pub segment_files: usize,
    /// Fault injection (tests/drills only): after this many files are
    /// written the pass stops as if the process were killed: no cleanup,
    /// the open segment is lost and the pending build claim stays.
    pub inject_crash_after_files: Option<usize>,
}

/// Default files per full-build segment.
/// Small enough that a crash loses little work (a resumed 8k-file build
/// redoes at most one segment plus what was never reached), large enough
/// that commits and checkpoints stay a negligible cost.
pub const SEGMENT_FILES: usize = 250;
/// Input bytes after which a full build commits a segment early.
pub const SEGMENT_BYTES: i64 = 64 * 1024 * 1024;
/// Error message of an injected crash ([`Options::inject_crash_after_files`]).
pub const INJECTED_CRASH: &str = "injected crash";

impl Options {
    pub fn from_config(cfg: &DocMonkConfig) -> Self {
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
        // Worker threads per pass may exceed the CPU budget: actual CPU use
        // is bounded process-wide by the resource governor's `cpu` permits
        // (shared by every concurrently running source), not per pass.
        let wanted = cfg
            .indexing
            .code_extraction_workers
            .max(cfg.indexing.document_extraction_workers)
            .max(cfg.indexing.resolved_cpu_workers() as i64)
            .max(1) as usize;
        Self {
            workers: wanted.min(cpus.max(1) * 2).max(1),
            max_file_size_bytes: cfg.indexing.max_file_size_mb.max(0) * 1024 * 1024,
            follow_symlinks: cfg.indexing.follow_symlinks,
            cache_size_mb: cfg.runtime.sqlite_cache_size_mb,
            lock_timeout: Duration::from_secs_f64(cfg.indexing.lock_timeout_seconds),
            inject_unreadable: Vec::new(),
            segment_files: SEGMENT_FILES,
            inject_crash_after_files: None,
        }
    }
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, false)
}

fn db_err(e: impl std::fmt::Display) -> DocMonkError {
    DocMonkError::new(ErrorKind::Database, e.to_string())
}

struct Work {
    input: PrepareInput,
    prev_attempts: i64,
    size: i64,
    mtime: f64,
    skip_limit: bool,
    /// Share of the process-wide in-flight byte budget, held from queueing
    /// until the writer has applied the result.
    budget: Option<crate::governor::Permit<'static>>,
}

struct Done {
    work: Work,
    outcome: Result<FileKnowledge, ProcessError>,
}

fn file_row(
    work: &Work,
    versions: &IndexVersions,
    status: &str,
    attempts: i64,
    next: Option<String>,
    err: Option<String>,
) -> FileRow {
    FileRow {
        id: work.input.file_id.clone(),
        rel_path: work.input.rel_path.clone(),
        kind: work.input.kind.as_str().into(),
        size: work.size,
        mtime: work.mtime,
        content_hash: work.input.content_hash.clone(),
        status: status.into(),
        parser_version: Some(versions.parser_version.clone()),
        chunker_version: Some(versions.chunker_version.clone()),
        converter_version: Some(versions.converter_version.clone()),
        embedding_model_id: versions.embedding_model_id.clone(),
        embedding_text_version: versions.embedding_text_version.clone(),
        last_error: err,
        attempt_count: attempts,
        next_attempt_at: next,
        last_indexed_at: None,
    }
}

/// Hashes `files` on up to `workers` threads: `path -> hash` (`None` when
/// unreadable).
fn hash_parallel(
    files: &[&crate::scan::ScannedFile],
    workers: usize,
) -> std::collections::HashMap<PathBuf, Option<String>> {
    use std::sync::atomic::AtomicUsize;
    if files.is_empty() {
        return Default::default();
    }
    let next = AtomicUsize::new(0);
    let threads = workers.clamp(1, files.len());
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(f) = files.get(i) else { break };
                        out.push((f.path.clone(), hash_file(&f.path).ok()));
                    }
                    out
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    })
}

/// Most files the writer commits in one transaction.
const WRITE_BATCH: usize = 64;

fn run_workers(
    items: Vec<Work>,
    workers: usize,
    registry: &Registry,
    mut on_done: impl FnMut(Vec<Done>) -> Result<(), DocMonkError>,
) -> Result<PipelineStats, DocMonkError> {
    use std::sync::atomic::AtomicUsize;
    let bound = workers.saturating_mul(2).max(1);
    let in_flight = AtomicUsize::new(0);
    let in_flight_high = AtomicUsize::new(0);
    let active = AtomicUsize::new(0);
    let active_high = AtomicUsize::new(0);
    let items_len = items.len();
    let (work_tx, work_rx) = sync_channel::<Work>(bound);
    let (done_tx, done_rx) = sync_channel::<Done>(bound);
    let work_rx: Arc<Mutex<Receiver<Work>>> = Arc::new(Mutex::new(work_rx));
    let cancel = AtomicBool::new(false);
    let mut first_err = None;
    std::thread::scope(|s| {
        for _ in 0..workers {
            let rx = Arc::clone(&work_rx);
            let tx = done_tx.clone();
            let cancel = &cancel;
            let (active, active_high) = (&active, &active_high);
            s.spawn(move || loop {
                let next = rx.lock().ok().and_then(|r| r.recv().ok());
                let Some(mut work) = next else { break };
                if cancel.load(Ordering::Relaxed) {
                    continue;
                }
                let outcome = if work.skip_limit {
                    Ok(FileKnowledge::default())
                } else {
                    // Governor order: bytes (held by the item) < ocr < cpu.
                    let gov = crate::governor::global();
                    let _ocr = (work.input.kind == FileKind::Document
                        && crate::governor::heavy_document(&work.input.rel_path))
                    .then(|| gov.acquire(crate::governor::Class::Ocr, 1));
                    let _cpu = gov.acquire(crate::governor::Class::Cpu, 1);
                    let now = active.fetch_add(1, Ordering::Relaxed) + 1;
                    active_high.fetch_max(now, Ordering::Relaxed);
                    // New files are hashed here, in parallel, not by the
                    // diff (an unreadable file is left to the processor,
                    // which reports the I/O error).
                    if work.input.content_hash.is_none() {
                        work.input.content_hash = hash_file(&work.input.path).ok();
                    }
                    let processor = registry.for_kind(work.input.kind);
                    let r = catch_unwind(AssertUnwindSafe(|| processor.prepare(&work.input)))
                        .unwrap_or_else(|_| {
                            Err(ProcessError {
                                code: "processor_panic".into(),
                                message: "processor panicked".into(),
                                transient: false,
                            })
                        });
                    active.fetch_sub(1, Ordering::Relaxed);
                    r
                };
                if tx.send(Done { work, outcome }).is_err() {
                    break;
                }
            });
        }
        drop(done_tx);
        let cancel_ref = &cancel;
        let (in_flight_ref, in_flight_high_ref) = (&in_flight, &in_flight_high);
        s.spawn(move || {
            for mut item in items {
                if cancel_ref.load(Ordering::Relaxed) {
                    break;
                }
                // Backpressure on estimated bytes across every running
                // source, not just item count.
                item.budget = Some(
                    crate::governor::global()
                        .acquire(crate::governor::Class::Bytes, item.size.max(1) as usize),
                );
                // Counted before the send so the writer never sees it negative.
                let now = in_flight_ref.fetch_add(1, Ordering::Relaxed) + 1;
                in_flight_high_ref.fetch_max(now, Ordering::Relaxed);
                if work_tx.send(item).is_err() {
                    break;
                }
            }
        });
        // The writer takes whatever results are already waiting (bounded by
        // the channel and WRITE_BATCH) and commits them in one transaction.
        while let Ok(first) = done_rx.recv() {
            in_flight.fetch_sub(1, Ordering::Relaxed);
            let mut batch = vec![first];
            while batch.len() < WRITE_BATCH {
                match done_rx.try_recv() {
                    Ok(d) => {
                        in_flight.fetch_sub(1, Ordering::Relaxed);
                        batch.push(d);
                    }
                    Err(_) => break,
                }
            }
            if first_err.is_some() {
                continue;
            }
            if let Err(e) = on_done(batch) {
                cancel.store(true, Ordering::Relaxed);
                first_err = Some(e);
            }
        }
    });
    match first_err {
        Some(e) => Err(e),
        None => Ok(PipelineStats {
            workers,
            queue_capacity: bound,
            in_flight_high_water: in_flight_high.into_inner(),
            active_workers_high_water: active_high.into_inner(),
            items: items_len,
        }),
    }
}

/// Indexes one source. Errors abort only this source's pending build.
pub fn run_source(
    layout: &StorageLayout,
    control: &mut ControlPlane,
    source: &SourceRecord,
    registry: &Registry,
    opts: &Options,
    progress: &mut dyn Progress,
) -> Result<SourceResult, DocMonkError> {
    run_source_with(layout, control, source, registry, opts, None, progress)
}

/// [`run_source`], optionally limited to `targets` (absolute paths a
/// watcher reported). A targeted pass examines only those paths and
/// diffs them against their own baseline rows. Rows for other files are
/// left untouched. Deletions are therefore never inferred from absence,
/// and a rename is kept as a move only when both halves are named.
/// Targets are ignored when the plan is a full rebuild, which must see
/// the whole tree.
pub fn run_source_with(
    layout: &StorageLayout,
    control: &mut ControlPlane,
    source: &SourceRecord,
    registry: &Registry,
    opts: &Options,
    targets: Option<&std::collections::BTreeSet<PathBuf>>,
    progress: &mut dyn Progress,
) -> Result<SourceResult, DocMonkError> {
    let mut result = SourceResult {
        source_id: source.id.clone(),
        ..SourceResult::default()
    };
    let root = Path::new(&source.path);
    if let Some(reason) = check_root_accessible(root) {
        control
            .set_online(&source.id, false, Some(&reason))
            .map_err(db_err)?;
        result.offline = Some(reason);
        return Ok(result);
    }
    let was_offline = control
        .state(&source.id)
        .map_err(db_err)?
        .online_status
        .eq_ignore_ascii_case("offline");
    control.set_online(&source.id, true, None).map_err(db_err)?;
    result.became_online = was_offline;
    let project_id = project_id_for_canonical(&source.path);
    let mut store =
        ProjectStore::open(layout, &project_id, &source.id, opts.cache_size_mb).map_err(db_err)?;
    let mut state = control.state(&source.id).map_err(db_err)?;
    let mut resume: Option<String> = None;
    if let Some(pending) = state.pending_build_id.clone().filter(|p| {
        state.active_build_id.as_deref() != Some(p.as_str())
            && store
                .resumable_build(p, &registry.versions)
                .unwrap_or(false)
    }) {
        // A full build of these exact versions died part-way. Callers hold
        // the `index` lock, so nothing is in flight: keep its committed
        // segments (still invisible) and finish it.
        store
            .gc_builds_keeping(state.active_build_id.as_deref(), Some(&pending))
            .map_err(db_err)?;
        tracing::info!(component = "indexing", event = "build_resumed", source_id = %source.id, build_id = %pending);
        result.resumed_build = Some(pending.clone());
        resume = Some(pending);
    } else if let Some(pending) = state.pending_build_id.clone() {
        // A previous run died mid-build. Callers hold the `index` lock, so
        // nothing is in flight: drop its invisible rows and its claim, then
        // plan from the last published build.
        result.recovered_build = Some(pending.clone());
        store
            .gc_builds(state.active_build_id.as_deref())
            .map_err(db_err)?;
        control
            .abort_build(
                &source.id,
                &pending,
                "the previous run stopped before publishing; its build was discarded",
            )
            .map_err(db_err)?;
        if state.active_build_id.as_deref() != Some(pending.as_str()) {
            // It was a separate full build: redo it rather than fall back
            // to an incremental pass over the old one.
            control
                .require_full_rebuild(&source.id, "the previous full build did not finish")
                .map_err(db_err)?;
        }
        state = control.state(&source.id).map_err(db_err)?;
    }
    let plan = match &resume {
        Some(_) => RebuildPlan::Full {
            reason: "resuming an interrupted full build".into(),
        },
        None => plan_for(&state, &registry.versions),
    };
    result.plan = match &plan {
        RebuildPlan::Full { reason } => format!("full ({reason})"),
        RebuildPlan::Incremental { .. } => "incremental".into(),
    };

    progress.event(&ProgressEvent::Stage {
        source_id: source.id.clone(),
        stage: "scanning",
    });
    let started = Instant::now();
    let ignore = IgnoreMatcher::new(
        &docmonk_core::paths::resolve(root).map_err(db_err)?,
        &source.exclude_patterns,
        &source.include_patterns,
    );
    let targets = targets.filter(|_| matches!(plan, RebuildPlan::Incremental { .. }));
    let (scanned, missing) = match targets {
        Some(t) => {
            let ts = scan_targets(root, &ignore, t, opts.follow_symlinks).map_err(|e| {
                DocMonkError::new(ErrorKind::SourceUnavailable, format!("scan failed: {e}"))
            })?;
            result.targeted = true;
            (ts.present, Some(ts.missing))
        }
        None => (
            scan(
                root,
                &ignore,
                &ScanOptions {
                    follow_symlinks: opts.follow_symlinks,
                    inject_unreadable: opts.inject_unreadable.clone(),
                },
            )
            .map_err(|e| {
                DocMonkError::new(ErrorKind::SourceUnavailable, format!("scan failed: {e}"))
            })?,
            None,
        ),
    };
    result.timings.scan_seconds = started.elapsed().as_secs_f64();
    result.scan_errors = scanned
        .errors
        .iter()
        .map(|e| format!("{}: {}", e.path, e.message))
        .collect();
    if !scanned.complete() {
        tracing::warn!(component = "indexing", event = "scan_incomplete", source_id = %source.id, errors = scanned.errors.len());
    }

    let started = Instant::now();
    // A resumed build diffs against its own committed files: those whose
    // stat still matches are already done.
    let mut baseline = match &resume {
        Some(pending) => store.files(pending),
        None => store.reusable_files(&plan),
    }
    .map_err(db_err)?;
    if let Some(missing) = &missing {
        // Only the named paths take part: rows for every other file are
        // neither diffed nor touched.
        let named: std::collections::HashSet<&str> = scanned
            .files
            .iter()
            .map(|f| f.rel_path.as_str())
            .chain(missing.iter().map(String::as_str))
            .collect();
        baseline.retain(|r| named.contains(r.rel_path.as_str()));
    }
    let now = now_iso();
    let prehashed = hash_parallel(&hash_candidates(&scanned, &baseline, &now), opts.workers);
    let mut d = diff(&scanned, &baseline, &now, &mut |p| match prehashed.get(p) {
        Some(Some(h)) => Ok(h.clone()),
        Some(None) => Err(std::io::Error::other("unreadable")),
        None => hash_file(p),
    });
    if let Some(missing) = &missing {
        d.counts.scanned += missing.len();
    }
    result.timings.classify_seconds = started.elapsed().as_secs_f64();
    result.timings.hash_calls = d.counts.hash_calls;
    result.counts = d.counts.clone();
    let to_process: Vec<&Decision> = d.to_process().collect();
    progress.event(&ProgressEvent::Scanned {
        source_id: source.id.clone(),
        files: d.counts.scanned,
        to_process: to_process.len(),
    });

    // Warm pass: nothing to rebuild. Refresh stale stats in place on the
    // active build (content proven identical by hash) and stop.
    if let RebuildPlan::Incremental { active_build_id } = &plan {
        if to_process.is_empty() && d.counts.deleted == 0 {
            for dec in d
                .decisions
                .iter()
                .filter(|x| x.change == Change::UnchangedRestat)
            {
                if let (Some(prev), Some(sf)) = (&dec.prev, &dec.scanned) {
                    store
                        .set_file_stat(active_build_id, &prev.id, sf.size, sf.mtime)
                        .map_err(db_err)?;
                }
            }
            for finalizer in &registry.finalizers {
                let report = finalizer
                    .on_warm_pass(&mut store, active_build_id)
                    .map_err(|e| {
                        DocMonkError::new(
                            ErrorKind::Generic,
                            format!("warm pass repair failed ({}): {}", e.code, e.message),
                        )
                    })?;
                result.linked += report.linked;
                result.embedded += report.embedded;
            }
            result.build_id = Some(active_build_id.clone());
            return Ok(result);
        }
    }

    // Every build runs inside SQLite sessions, so each B-tree page is
    // written about once. A full rebuild writes a new build next to the
    // visible one, committing it in segments, and switches over at
    // publish; an incremental pass updates the active build in place in
    // one session, and readers keep the committed state until COMMIT.
    // Incomplete work is never visible either way.
    let (build_id, full) = match &plan {
        RebuildPlan::Full { .. } if resume.is_some() => (resume.clone().unwrap_or_default(), true),
        RebuildPlan::Full { .. } => (
            record::build_id(&source.id, &format!("{}-{}", now, std::process::id())),
            true,
        ),
        RebuildPlan::Incremental { active_build_id } => (active_build_id.clone(), false),
    };
    control.begin_build(&source.id, &build_id).map_err(db_err)?;
    result.build_id = Some(build_id.clone());
    if let Err(e) = store.begin_session() {
        let _ = control.abort_build(&source.id, &build_id, &e.to_string());
        return Err(db_err(e));
    }
    let outcome = build(
        &mut store,
        &build_id,
        full,
        resume.is_some(),
        &plan,
        &d.decisions,
        &to_process,
        &source.id,
        registry,
        opts,
        progress,
        &mut result,
    );
    let outcome = outcome.and_then(|()| {
        progress.event(&ProgressEvent::Stage {
            source_id: source.id.clone(),
            stage: "publishing",
        });
        let started = Instant::now();
        if full {
            // Committed (its last segment before the finalizers) but still
            // invisible: the control plane points at the previous build
            // until publish_build switches it.
            if store.in_session() {
                store.commit_session().map_err(db_err)?;
            }
            control
                .publish_build(&source.id, &build_id, &registry.versions, full)
                .map_err(db_err)?;
            store.mark_published(&build_id).map_err(db_err)?;
            // The relationship graph is reused across base builds: carry
            // its rows into the new build before the old one is collected
            // (still invisible; the graph stage validates the snapshot).
            // A failure only costs a full graph derivation.
            if let Some(prev) = state.active_build_id.as_deref() {
                if let Err(e) = store.carry_graph_forward(prev, &build_id) {
                    tracing::warn!(component = "indexing", event = "graph_not_carried", source_id = %source.id, error = %e);
                }
            }
            store.gc_builds(Some(&build_id)).map_err(db_err)?;
        } else {
            store.touch_build(&build_id).map_err(db_err)?;
            store.commit_session().map_err(db_err)?;
            // Already committed and consistent: if this fails or the process
            // dies here, the next pass diffs against the updated build.
            control
                .publish_build(&source.id, &build_id, &registry.versions, full)
                .map_err(db_err)?;
        }
        result.timings.publish_seconds = started.elapsed().as_secs_f64();
        Ok(())
    });
    match outcome {
        Ok(()) => {
            result.published = true;
            Ok(result)
        }
        Err(e) if e.message() == INJECTED_CRASH => {
            // As if killed: the open segment is lost with the connection,
            // committed segments and the pending claim stay.
            drop(store);
            Err(e)
        }
        Err(e) => {
            let _ = control.abort_build(&source.id, &build_id, e.message());
            if store.in_session() {
                let _ = store.rollback_session();
            }
            if full {
                // Committed segments (or a committed build that failed to
                // publish) are discarded; the previous build stays.
                let _ = store.mark_aborted(&build_id);
                let _ = store.gc_builds(state.active_build_id.as_deref());
            }
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build(
    store: &mut ProjectStore,
    build_id: &str,
    full: bool,
    resume: bool,
    plan: &RebuildPlan,
    decisions: &[Decision],
    to_process: &[&Decision],
    source_id: &str,
    registry: &Registry,
    opts: &Options,
    progress: &mut dyn Progress,
    result: &mut SourceResult,
) -> Result<(), DocMonkError> {
    if full && !resume {
        store
            .create_build(build_id, full, &registry.versions)
            .map_err(db_err)?;
    }
    let mut touched: Vec<String> = Vec::new();
    if !full || resume {
        if let RebuildPlan::Incremental { active_build_id } = plan {
            debug_assert_eq!(active_build_id, build_id);
        }
        // In place (an incremental pass, or a resumed full build): drop
        // deleted and moved-away files, refresh restat-only stats;
        // reprocessed files replace their own rows when written.
        let mut exclude = Vec::new();
        let mut restat = Vec::new();
        for dec in decisions {
            if resume
                && matches!(
                    dec.change,
                    Change::Unchanged | Change::Unseen | Change::UnchangedRestat
                )
            {
                // Already written by the interrupted run: still part of
                // what the full build's finalizers must cover.
                if let Some(prev) = &dec.prev {
                    touched.push(prev.id.clone());
                }
            }
            match dec.change {
                Change::Unchanged | Change::Unseen => result.carried_forward += 1,
                Change::UnchangedRestat => {
                    result.carried_forward += 1;
                    if let (Some(prev), Some(sf)) = (&dec.prev, &dec.scanned) {
                        restat.push((prev.id.clone(), sf.size, sf.mtime));
                    }
                }
                Change::Deleted => {
                    if let Some(prev) = &dec.prev {
                        exclude.push(prev.id.clone());
                    }
                }
                Change::Changed => {}
                Change::Moved => {
                    if let Some(old) = &dec.moved_from {
                        exclude.push(old.id.clone());
                    }
                }
                Change::New => {}
            }
        }
        for file_id in &exclude {
            store.remove_file(build_id, file_id).map_err(db_err)?;
        }
        for (file_id, size, mtime) in &restat {
            store
                .set_file_stat(build_id, file_id, *size, *mtime)
                .map_err(db_err)?;
        }
    }
    progress.event(&ProgressEvent::Stage {
        source_id: source_id.into(),
        stage: "processing",
    });
    let started = Instant::now();
    let total = to_process.len();
    let items: Vec<Work> = to_process
        .iter()
        .filter_map(|dec| {
            let sf = dec.scanned.as_ref()?;
            Some(Work {
                input: PrepareInput {
                    source_id: source_id.into(),
                    file_id: record::file_id(source_id, &dec.rel_path),
                    rel_path: dec.rel_path.clone(),
                    path: sf.path.clone(),
                    kind: dec.kind,
                    content_hash: dec.content_hash.clone(),
                },
                prev_attempts: dec.prev.as_ref().map_or(0, |p| p.attempt_count),
                size: sf.size,
                mtime: sf.mtime,
                skip_limit: opts.max_file_size_bytes > 0 && sf.size > opts.max_file_size_bytes,
                budget: None,
            })
        })
        .collect();
    let mut done = 0usize;
    let (mut segment_files, mut segment_bytes) = (0usize, 0i64);
    let pipeline = run_workers(items, opts.workers, registry, |batch| {
        segment_files += batch.len();
        segment_bytes += batch.iter().map(|d| d.work.size.max(0)).sum::<i64>();
        let mut rows = Vec::with_capacity(batch.len());
        let mut outcomes = Vec::with_capacity(batch.len());
        for d in batch {
            let versions = &registry.versions;
            let (row, knowledge, outcome) = match d.outcome {
                Ok(_) if d.work.skip_limit => {
                    result.skipped_limit += 1;
                    (
                        file_row(&d.work, versions, "skipped_limit", 0, None, None),
                        FileKnowledge::default(),
                        FileOutcome::Skipped,
                    )
                }
                Ok(k) => {
                    result.indexed += 1;
                    result.attachments += k.attachments;
                    (
                        file_row(&d.work, versions, "indexed", 0, None, None),
                        k,
                        FileOutcome::Indexed,
                    )
                }
                Err(e) if e.code == SKIPPED_LIMIT => {
                    result.skipped_limit += 1;
                    (
                        file_row(&d.work, versions, "skipped_limit", 0, None, Some(e.message)),
                        FileKnowledge::default(),
                        FileOutcome::Skipped,
                    )
                }
                Err(e) => {
                    let attempts = d.work.prev_attempts + 1;
                    let permanent = !e.transient || retry::is_permanent(attempts);
                    let (status, next, outcome) = if permanent {
                        result.failed += 1;
                        ("failed", None, FileOutcome::Failed)
                    } else {
                        result.retrying += 1;
                        (
                            "retry",
                            Some(retry::next_attempt_at(attempts, chrono::Utc::now())),
                            FileOutcome::Retry,
                        )
                    };
                    store
                        .record_error(
                            Some(build_id),
                            Some(&d.work.input.file_id),
                            Some(&d.work.input.rel_path),
                            &e.code,
                            &e.message,
                        )
                        .map_err(db_err)?;
                    (
                        file_row(&d.work, versions, status, attempts, next, Some(e.message)),
                        FileKnowledge::default(),
                        outcome,
                    )
                }
            };
            rows.push((row, knowledge));
            outcomes.push(outcome);
        }
        touched.extend(rows.iter().map(|(r, _)| r.id.clone()));
        let refs: Vec<(&FileRow, &FileKnowledge)> = rows.iter().map(|(r, k)| (r, k)).collect();
        store.put_files(build_id, &refs).map_err(db_err)?;
        for outcome in outcomes {
            done += 1;
            progress.event(&ProgressEvent::FileDone {
                source_id: source_id.into(),
                done,
                total,
                outcome,
            });
        }
        if full && (segment_files >= opts.segment_files.max(1) || segment_bytes >= SEGMENT_BYTES) {
            store.commit_segment().map_err(db_err)?;
            result.segments += 1;
            (segment_files, segment_bytes) = (0, 0);
        }
        if opts.inject_crash_after_files.is_some_and(|n| done >= n) {
            return Err(DocMonkError::new(ErrorKind::Generic, INJECTED_CRASH));
        }
        Ok(())
    })?;
    result.pipeline = pipeline;
    if full {
        // The build is still invisible: commit its last segment so the
        // finalizers' model inference runs with no transaction open (each
        // window of vectors is written in its own short transaction).
        store.commit_session().map_err(db_err)?;
    }
    if !registry.finalizers.is_empty() {
        progress.event(&ProgressEvent::Stage {
            source_id: source_id.into(),
            stage: "finalizing",
        });
    }
    for finalizer in &registry.finalizers {
        let report = finalizer.finalize(store, build_id, &touched).map_err(|e| {
            DocMonkError::new(
                ErrorKind::Generic,
                format!("build finalization failed ({}): {}", e.code, e.message),
            )
        })?;
        result.linked += report.linked;
        result.embedded += report.embedded;
    }
    result.timings.process_seconds = started.elapsed().as_secs_f64();
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RunSummary {
    pub sources: Vec<SourceResult>,
    /// `(source_id, error)` for sources whose pass failed.
    pub failures: Vec<(String, String)>,
}

/// Indexes every enabled source under the bounded `locks/index.lock`.
/// One source failing never stops the others.
pub fn index_all(
    home: &Home,
    control: &mut ControlPlane,
    registry: &Registry,
    opts: &Options,
    progress: &mut dyn Progress,
) -> Result<RunSummary, DocMonkError> {
    let lock = RunLock::acquire(
        &home.locks_dir().join("index.lock"),
        "index",
        None,
        opts.lock_timeout,
    )?;
    let layout = StorageLayout::new(home);
    let sources = control.list_sources(true).map_err(db_err)?;
    let mut summary = RunSummary::default();
    let total = sources.len();
    for (i, source) in sources.iter().enumerate() {
        progress.event(&ProgressEvent::SourceStarted {
            source_id: source.id.clone(),
            position: i + 1,
            total,
        });
        match run_source(&layout, control, source, registry, opts, progress) {
            Ok(r) => {
                progress.event(&ProgressEvent::SourceFinished {
                    source_id: source.id.clone(),
                    ok: true,
                });
                summary.sources.push(r);
            }
            Err(e) => {
                tracing::warn!(component = "indexing", event = "source_failed", source_id = %source.id, error = %e.message());
                progress.event(&ProgressEvent::SourceFinished {
                    source_id: source.id.clone(),
                    ok: false,
                });
                summary
                    .failures
                    .push((source.id.clone(), e.message().to_owned()));
            }
        }
    }
    lock.release();
    Ok(summary)
}
