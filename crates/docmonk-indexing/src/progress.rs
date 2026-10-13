//! Live indexing progress snapshot (`<home>/index_progress.json`).
//!
//! One run (CLI `index`/`rebuild`, the daemon, the Admin UI runner) owns
//! the snapshot. It tracks **every source pass separately**: parallel
//! passes never overwrite each other's stage or counters, and a pass's
//! entry keeps its own run id, stage, planned/processed counters, last
//! meaningful progress and heartbeat.
//!
//! * The file format is versioned ([`SCHEMA_VERSION`]); the reader returns
//!   `None` for any other version, a missing or a malformed file.
//! * Writes are atomic (temp file + rename) and coalesced: counter updates
//!   at most once per [`MIN_WRITE_INTERVAL`]; source/stage changes and the
//!   final snapshot are always written.
//! * **Heartbeats are not progress.** An independent ticker refreshes
//!   `heartbeat_at` every [`HEARTBEAT_INTERVAL`] while the run is alive, so
//!   a long OCR, embedding or linking stage stays visibly alive; only real
//!   events move `last_progress_at` and the counters.
//! * Every run ends with a terminal outcome (`completed`, `failed` or
//!   `interrupted`); a crash leaves `running: true` with a heartbeat that
//!   stops, which status reads through the process probe.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::coordinator::{FileOutcome, Progress, ProgressEvent};

pub const SCHEMA_VERSION: i64 = 2;
/// Minimum spacing of coalesced (counter-only) writes.
pub const MIN_WRITE_INTERVAL: Duration = Duration::from_secs(1);
/// Spacing of the independent liveness heartbeat.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// Finished source entries kept in the snapshot (active ones are always kept).
pub const MAX_FINISHED_SOURCES: usize = 256;

/// Canonical stage names, in pass order. `indexed` (base index published,
/// relationship graph pending) and `relationships` (graph stage running)
/// only appear when `indexing.relationships_enabled` is on: the graph stage
/// starts after every selected source finished indexing.
pub const STAGES: [&str; 8] = [
    "starting",
    "scanning",
    "processing",
    "finalizing",
    "publishing",
    "indexed",
    "relationships",
    "done",
];

/// Run phases, in order: every selected source is indexed (`indexing`),
/// then the barrier (`indexed`), then the graph stage (`relationships`,
/// skipped when disabled), then `done`.
pub const PHASES: [&str; 4] = ["indexing", "indexed", "relationships", "done"];

fn is_zero(v: &i64) -> bool {
    *v == 0
}

/// One source pass inside a run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceProgress {
    pub source_id: String,
    /// The pass is running right now.
    pub active: bool,
    pub stage: Option<String>,
    /// `completed` or `failed` once the pass ended.
    pub outcome: Option<String>,
    pub scanned: i64,
    /// Files this pass must process; unknown until the scan finished.
    pub planned: Option<i64>,
    pub processed: i64,
    pub indexed: i64,
    pub failed: i64,
    pub retry: i64,
    pub started_at: Option<String>,
    /// Last stage change, scan result or file completion.
    pub last_progress_at: Option<String>,
    /// Last liveness signal (progress or heartbeat).
    pub heartbeat_at: Option<String>,
    pub finished_at: Option<String>,
    /// Relationship graph stage of this source, independent of indexing:
    /// `pending`, `building`, `ready`, `up_to_date`, `failed`, `stale`,
    /// `skipped` or `disabled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relationships: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relationships_planned: Option<i64>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub relationships_processed: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relationships_error: Option<String>,
}

impl SourceProgress {
    pub fn new(source_id: &str, now: &str) -> Self {
        Self {
            source_id: source_id.to_owned(),
            active: true,
            stage: Some("starting".into()),
            started_at: Some(now.to_owned()),
            last_progress_at: Some(now.to_owned()),
            heartbeat_at: Some(now.to_owned()),
            ..Self::default()
        }
    }

    /// Applies one coordinator event addressed to this source. Returns
    /// whether it was a structural change (always written).
    pub fn apply(&mut self, event: &ProgressEvent, now: &str) -> bool {
        let touch = |p: &mut Self| {
            p.last_progress_at = Some(now.to_owned());
            p.heartbeat_at = Some(now.to_owned());
        };
        match event {
            ProgressEvent::SourceStarted { .. } => {
                *self = Self::new(&self.source_id, now);
                true
            }
            ProgressEvent::Stage { stage, .. } => {
                let changed = self.stage.as_deref() != Some(stage);
                self.stage = Some((*stage).to_owned());
                touch(self);
                changed
            }
            ProgressEvent::Scanned {
                files, to_process, ..
            } => {
                self.scanned = *files as i64;
                self.planned = Some(*to_process as i64);
                touch(self);
                true
            }
            ProgressEvent::FileDone { outcome, .. } => {
                self.processed += 1;
                match outcome {
                    FileOutcome::Indexed => self.indexed += 1,
                    FileOutcome::Failed => self.failed += 1,
                    FileOutcome::Retry => self.retry += 1,
                    FileOutcome::Skipped => {}
                }
                touch(self);
                false
            }
            ProgressEvent::Heartbeat => {
                self.heartbeat_at = Some(now.to_owned());
                false
            }
            ProgressEvent::SourceFinished { ok, .. } => {
                self.finish(*ok, now);
                true
            }
        }
    }

    pub fn finish(&mut self, ok: bool, now: &str) {
        self.active = false;
        self.outcome = Some(if ok { "completed" } else { "failed" }.into());
        self.stage = Some("done".into());
        self.finished_at = Some(now.to_owned());
        self.heartbeat_at = Some(now.to_owned());
    }
}

/// The source a coordinator event is about (`None` for heartbeats).
pub fn event_source(event: &ProgressEvent) -> Option<&str> {
    match event {
        ProgressEvent::SourceStarted { source_id, .. }
        | ProgressEvent::Stage { source_id, .. }
        | ProgressEvent::Scanned { source_id, .. }
        | ProgressEvent::FileDone { source_id, .. }
        | ProgressEvent::SourceFinished { source_id, .. } => Some(source_id),
        ProgressEvent::Heartbeat => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexProgress {
    pub schema_version: i64,
    pub running: bool,
    pub run_id: Option<String>,
    pub pid: Option<i64>,
    pub host: Option<String>,
    pub operation: Option<String>,
    /// `running`, `completed`, `failed` or `interrupted`.
    pub outcome: String,
    pub started_at: Option<String>,
    /// Last write (progress or heartbeat).
    pub updated_at: Option<String>,
    pub completed_at: Option<String>,
    /// Sources this run was asked to cover, when known up front.
    pub source_total: Option<i64>,
    pub max_parallel_sources: Option<i64>,
    pub sources_completed: i64,
    pub sources_failed: i64,
    /// Every source pass of this run, active ones first.
    pub sources: Vec<SourceProgress>,
    pub error: Option<String>,
    /// Resource-governor counters per permit class.
    pub resources: Option<Value>,
    /// The run's phase (see [`PHASES`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
}

impl Default for IndexProgress {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            running: false,
            run_id: None,
            pid: None,
            host: None,
            operation: None,
            outcome: "running".into(),
            started_at: None,
            updated_at: None,
            completed_at: None,
            source_total: None,
            max_parallel_sources: None,
            sources_completed: 0,
            sources_failed: 0,
            sources: Vec::new(),
            error: None,
            resources: None,
            phase: None,
        }
    }
}

impl IndexProgress {
    /// Sources whose pass is running right now.
    pub fn active_sources(&self) -> impl Iterator<Item = &SourceProgress> {
        self.sources.iter().filter(|s| s.active)
    }

    pub fn source(&self, source_id: &str) -> Option<&SourceProgress> {
        self.sources.iter().find(|s| s.source_id == source_id)
    }

    fn source_mut(&mut self, source_id: &str, now: &str) -> &mut SourceProgress {
        if let Some(i) = self.sources.iter().position(|s| s.source_id == source_id) {
            return &mut self.sources[i];
        }
        self.sources.push(SourceProgress::new(source_id, now));
        self.sources.last_mut().expect("just pushed")
    }

    /// Active entries first (by id), then the most recently finished ones.
    fn normalize(&mut self) {
        self.sources.sort_by(|a, b| {
            b.active
                .cmp(&a.active)
                .then_with(|| b.finished_at.cmp(&a.finished_at))
                .then_with(|| a.source_id.cmp(&b.source_id))
        });
        let active = self.sources.iter().filter(|s| s.active).count();
        self.sources.truncate(active + MAX_FINISHED_SOURCES);
    }
}

/// The clock progress timestamps come from (tests use a fake one).
pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// The system clock.
pub fn system_clock() -> Clock {
    Arc::new(Utc::now)
}

/// ISO-8601 UTC with microseconds and `+00:00`.
pub fn iso(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string()
}

pub fn now_iso() -> String {
    iso(Utc::now())
}

/// Writes `progress` atomically to `path`.
pub fn write_progress(path: &Path, progress: &IndexProgress) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = PathBuf::from(format!(
        "{}.{}.{:?}.tmp",
        path.display(),
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::write(
        &tmp,
        serde_json::to_vec(progress).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(&tmp, path)
}

/// Reads a snapshot; `None` when missing, malformed or of another schema.
pub fn read_progress(path: &Path) -> Option<IndexProgress> {
    let data: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    if data.get("schema_version").and_then(Value::as_i64) != Some(SCHEMA_VERSION) {
        return None;
    }
    serde_json::from_value(data).ok()
}

struct Inner {
    path: PathBuf,
    progress: IndexProgress,
    last_write: Option<Instant>,
    clock: Clock,
}

impl Inner {
    fn now(&self) -> String {
        iso((self.clock)())
    }

    fn flush(&mut self, force: bool) {
        if !force
            && self
                .last_write
                .is_some_and(|t| t.elapsed() < MIN_WRITE_INTERVAL)
        {
            return;
        }
        self.progress.updated_at = Some(self.now());
        if self.progress.running {
            self.progress.resources = Some(crate::governor::global().snapshot());
        }
        self.progress.normalize();
        self.last_write = Some(Instant::now());
        if let Err(e) = write_progress(&self.path, &self.progress) {
            tracing::debug!(component = "progress", event = "write_failed", error = %e);
        }
    }

    fn beat(&mut self) {
        let now = self.now();
        for s in self.progress.sources.iter_mut().filter(|s| s.active) {
            s.heartbeat_at = Some(now.clone());
        }
    }
}

/// Owns one run's snapshot and persists it. Cheap to clone; clones share
/// the snapshot (parallel source workers, finalizer heartbeats).
#[derive(Clone)]
pub struct ProgressTracker {
    inner: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for ProgressTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgressTracker").finish_non_exhaustive()
    }
}

impl ProgressTracker {
    /// Starts tracking `operation`, writes the first snapshot and starts
    /// the liveness ticker.
    pub fn start(path: impl Into<PathBuf>, operation: &str, source_total: Option<i64>) -> Self {
        Self::start_with_clock(path, operation, source_total, system_clock())
    }

    pub fn start_with_clock(
        path: impl Into<PathBuf>,
        operation: &str,
        source_total: Option<i64>,
        clock: Clock,
    ) -> Self {
        let now = iso(clock());
        let mut inner = Inner {
            path: path.into(),
            progress: IndexProgress {
                running: true,
                pid: Some(i64::from(std::process::id())),
                host: Some(crate::runtime::host_name()),
                operation: Some(operation.into()),
                started_at: Some(now.clone()),
                updated_at: Some(now),
                source_total,
                ..IndexProgress::default()
            },
            last_write: None,
            clock,
        };
        inner.flush(true);
        let tracker = Self {
            inner: Arc::new(Mutex::new(inner)),
        };
        spawn_ticker(Arc::downgrade(&tracker.inner));
        tracker
    }

    fn with<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut g)
    }

    /// The current snapshot.
    pub fn snapshot(&self) -> IndexProgress {
        self.with(|i| i.progress.clone())
    }

    /// One source's entry.
    pub fn source(&self, source_id: &str) -> Option<SourceProgress> {
        self.with(|i| i.progress.source(source_id).cloned())
    }

    /// Records the run id and its parallelism cap.
    pub fn set_run(&self, run_id: &str, max_parallel_sources: usize) {
        self.with(|i| {
            i.progress.run_id = Some(run_id.into());
            i.progress.max_parallel_sources = Some(max_parallel_sources as i64);
            i.flush(true);
        });
    }

    /// A source pass started: its own entry, independent of the others.
    pub fn source_started(&self, source_id: &str) {
        self.with(|i| {
            let now = i.now();
            *i.progress.source_mut(source_id, &now) = SourceProgress::new(source_id, &now);
            i.flush(true);
        });
    }

    /// A source pass ended.
    pub fn source_finished(&self, source_id: &str, ok: bool) {
        self.with(|i| {
            let now = i.now();
            let entry = i.progress.source_mut(source_id, &now);
            let was_active = entry.active;
            entry.finish(ok, &now);
            if was_active {
                if ok {
                    i.progress.sources_completed += 1;
                } else {
                    i.progress.sources_failed += 1;
                }
            }
            i.flush(true);
        });
    }

    /// Moves the run to `phase` (see [`PHASES`]).
    pub fn set_phase(&self, phase: &str) {
        self.with(|i| {
            i.progress.phase = Some(phase.into());
            i.flush(true);
        });
    }

    /// Records a source's relationship graph stage (`progress` is files
    /// done / to do). The source's own stage follows it: `indexed` while
    /// pending, `relationships` while building, `done` once it ended.
    pub fn source_relationships(
        &self,
        source_id: &str,
        state: &str,
        progress: Option<(usize, usize)>,
        error: Option<&str>,
    ) {
        self.with(|i| {
            let now = i.now();
            let entry = i.progress.source_mut(source_id, &now);
            let structural = entry.relationships.as_deref() != Some(state);
            entry.relationships = Some(state.into());
            entry.stage = Some(
                match state {
                    "pending" => "indexed",
                    "building" => "relationships",
                    _ => "done",
                }
                .into(),
            );
            if let Some((done, total)) = progress {
                entry.relationships_processed = done as i64;
                entry.relationships_planned = Some(total as i64);
            }
            if error.is_some() || structural {
                entry.relationships_error = error.map(str::to_owned);
            }
            entry.last_progress_at = Some(now.clone());
            entry.heartbeat_at = Some(now);
            i.flush(structural);
        });
    }

    /// Stores a final resource snapshot.
    pub fn set_resources(&self, resources: &Value) {
        self.with(|i| {
            i.progress.resources = Some(resources.clone());
        });
    }

    /// Liveness signal (no progress): refreshes every active source's
    /// heartbeat, coalesced like counter updates.
    pub fn heartbeat(&self) {
        self.with(|i| {
            i.beat();
            i.flush(false);
        });
    }

    /// Final snapshot: `completed`, `interrupted` (error starting with
    /// `interrupted`) or `failed` with `error`. Sources still marked active
    /// end with the run's outcome.
    pub fn finish(&self, error: Option<&str>) {
        self.with(|i| {
            let now = i.now();
            let p = &mut i.progress;
            p.running = false;
            p.outcome = match error {
                None => "completed",
                Some(e) if e.starts_with("interrupted") => "interrupted",
                Some(_) => "failed",
            }
            .into();
            p.error = error.map(|e| e.chars().take(500).collect());
            p.completed_at = Some(now.clone());
            for s in p.sources.iter_mut().filter(|s| s.active) {
                s.finish(error.is_none(), &now);
            }
            i.flush(true);
        });
    }
}

/// Refreshes the heartbeat every [`HEARTBEAT_INTERVAL`] until the run
/// finished or every tracker handle was dropped.
fn spawn_ticker(inner: Weak<Mutex<Inner>>) {
    let spawned = std::thread::Builder::new()
        .name("ragmonk-progress-heartbeat".into())
        .spawn(move || loop {
            let mut waited = Duration::ZERO;
            while waited < HEARTBEAT_INTERVAL {
                std::thread::sleep(Duration::from_millis(250));
                waited += Duration::from_millis(250);
                if inner.strong_count() == 0 {
                    return;
                }
            }
            let Some(inner) = inner.upgrade() else {
                return;
            };
            let mut g = inner.lock().unwrap_or_else(|p| p.into_inner());
            if !g.progress.running {
                return;
            }
            g.beat();
            g.flush(true);
        });
    if let Err(e) = spawned {
        tracing::debug!(component = "progress", event = "ticker_failed", error = %e);
    }
}

impl Progress for ProgressTracker {
    fn event(&mut self, event: &ProgressEvent) {
        self.with(|i| {
            let now = i.now();
            let structural = match event_source(event) {
                Some(id) => {
                    let started = matches!(event, ProgressEvent::SourceStarted { .. });
                    let finished = matches!(event, ProgressEvent::SourceFinished { .. });
                    let entry = i.progress.source_mut(id, &now);
                    let was_active = entry.active;
                    let structural = entry.apply(event, &now);
                    if finished && was_active {
                        if let ProgressEvent::SourceFinished { ok: true, .. } = event {
                            i.progress.sources_completed += 1;
                        } else {
                            i.progress.sources_failed += 1;
                        }
                    }
                    structural || started
                }
                None => {
                    i.beat();
                    false
                }
            };
            i.flush(structural);
        });
    }
}

static ACTIVE: Mutex<Option<ProgressTracker>> = Mutex::new(None);

/// Registers `tracker` as the process's active run, so long stages that
/// have no access to the coordinator's progress sink (build finalizers)
/// can still report liveness through [`heartbeat`]. The guard unregisters
/// it when dropped.
pub fn activate(tracker: &ProgressTracker) -> ActiveGuard {
    *ACTIVE.lock().unwrap_or_else(|p| p.into_inner()) = Some(tracker.clone());
    ActiveGuard(())
}

/// Drops the active-run registration.
#[derive(Debug)]
pub struct ActiveGuard(());

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        *ACTIVE.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

/// Heartbeat for the active run, if any (a no-op otherwise).
pub fn heartbeat() {
    let active = ACTIVE.lock().unwrap_or_else(|p| p.into_inner()).clone();
    if let Some(t) = active {
        t.heartbeat();
    }
}

/// Runs `f` under a tracker for `operation`: the final snapshot is written
/// on success (`completed`) and on error (`failed: <message>`).
pub fn track<T, E: std::fmt::Display>(
    path: &Path,
    operation: &str,
    source_total: Option<i64>,
    f: impl FnOnce(&mut ProgressTracker) -> Result<T, E>,
) -> Result<T, E> {
    let mut tracker = ProgressTracker::start(path, operation, source_total);
    let _guard = activate(&tracker);
    let result = f(&mut tracker);
    match &result {
        Ok(_) => tracker.finish(None),
        Err(e) => tracker.finish(Some(&e.to_string())),
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};

    fn fake_clock() -> (Clock, Arc<AtomicI64>) {
        let secs = Arc::new(AtomicI64::new(1_800_000_000));
        let s = secs.clone();
        let clock: Clock = Arc::new(move || {
            DateTime::from_timestamp(s.load(Ordering::SeqCst), 0).expect("valid time")
        });
        (clock, secs)
    }

    fn ev_stage(id: &str, stage: &'static str) -> ProgressEvent {
        ProgressEvent::Stage {
            source_id: id.into(),
            stage,
        }
    }

    fn ev_done(id: &str, outcome: FileOutcome) -> ProgressEvent {
        ProgressEvent::FileDone {
            source_id: id.into(),
            done: 0,
            total: 0,
            outcome,
        }
    }

    #[test]
    fn round_trips_and_rejects_other_versions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index_progress.json");
        assert!(read_progress(&path).is_none());
        let p = IndexProgress {
            running: true,
            pid: Some(7),
            sources: vec![SourceProgress::new("s", "t")],
            ..IndexProgress::default()
        };
        write_progress(&path, &p).unwrap();
        assert_eq!(read_progress(&path).unwrap(), p);
        std::fs::write(&path, "not json").unwrap();
        assert!(read_progress(&path).is_none());
        std::fs::write(&path, r#"{"schema_version": 1, "running": true}"#).unwrap();
        assert!(read_progress(&path).is_none());
        let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "no temp files left");
    }

    #[test]
    fn four_parallel_sources_keep_distinct_stages_and_counters() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.json");
        let mut t = ProgressTracker::start(&path, "index", Some(4));
        t.set_run("run-1", 4);
        let ids = ["s1", "s2", "s3", "s4"];
        for id in ids {
            t.source_started(id);
        }
        t.event(&ProgressEvent::Scanned {
            source_id: "s1".into(),
            files: 10,
            to_process: 10,
        });
        t.event(&ev_stage("s1", "processing"));
        for _ in 0..3 {
            t.event(&ev_done("s1", FileOutcome::Indexed));
        }
        t.event(&ev_stage("s2", "scanning"));
        t.event(&ProgressEvent::Scanned {
            source_id: "s3".into(),
            files: 5,
            to_process: 2,
        });
        t.event(&ev_stage("s3", "finalizing"));
        t.event(&ev_stage("s4", "publishing"));
        t.event(&ev_done("s4", FileOutcome::Failed));
        t.event(&ev_stage("s1", "processing"));
        let p = t.snapshot();
        let view: Vec<_> = ids
            .iter()
            .map(|id| {
                let s = p.source(id).unwrap();
                (s.active, s.stage.clone().unwrap(), s.planned, s.processed)
            })
            .collect();
        assert_eq!(
            view,
            [
                (true, "processing".to_owned(), Some(10), 3),
                (true, "scanning".to_owned(), None, 0),
                (true, "finalizing".to_owned(), Some(2), 0),
                (true, "publishing".to_owned(), None, 1),
            ]
        );
        assert_eq!(p.active_sources().count(), 4);
        t.source_finished("s2", true);
        t.source_finished("s4", false);
        let p = read_progress(&path).unwrap();
        assert_eq!(p.active_sources().count(), 2);
        assert_eq!((p.sources_completed, p.sources_failed), (1, 1));
        assert_eq!(p.source("s4").unwrap().outcome.as_deref(), Some("failed"));
        assert_eq!(p.run_id.as_deref(), Some("run-1"));
        t.finish(None);
        let p = read_progress(&path).unwrap();
        assert!(!p.running);
        assert_eq!(p.active_sources().count(), 0);
    }

    #[test]
    fn long_stage_heartbeats_never_count_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.json");
        let (clock, secs) = fake_clock();
        let mut t = ProgressTracker::start_with_clock(&path, "index", None, clock);
        t.source_started("ocr");
        t.event(&ev_stage("ocr", "processing"));
        let before = t.source("ocr").unwrap();
        // Ten minutes of a single OCR job: heartbeats only.
        for _ in 0..120 {
            secs.fetch_add(5, Ordering::SeqCst);
            t.event(&ProgressEvent::Heartbeat);
        }
        let after = t.source("ocr").unwrap();
        assert_eq!(
            (after.processed, after.indexed, after.planned),
            (0, 0, None)
        );
        assert_eq!(after.last_progress_at, before.last_progress_at);
        assert!(after.heartbeat_at > before.heartbeat_at);
        assert_eq!(
            after.heartbeat_at.as_deref(),
            Some("2027-01-15T08:10:00.000000+00:00")
        );
    }

    #[test]
    fn coalesces_counters_but_always_writes_stage_changes_and_finish() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.json");
        let mut t = ProgressTracker::start(&path, "index", Some(2));
        t.source_started("s1");
        for _ in 0..50 {
            t.event(&ev_done("s1", FileOutcome::Indexed));
        }
        // Counter updates inside the interval are not yet on disk.
        assert_eq!(
            read_progress(&path).unwrap().source("s1").unwrap().indexed,
            0
        );
        t.event(&ev_stage("s1", "publishing"));
        assert_eq!(
            read_progress(&path).unwrap().source("s1").unwrap().indexed,
            50
        );
        t.event(&ev_done("s1", FileOutcome::Retry));
        t.finish(None);
        let done = read_progress(&path).unwrap();
        let s = done.source("s1").unwrap();
        assert_eq!(
            (
                done.running,
                done.outcome.as_str(),
                s.retry,
                s.stage.as_deref()
            ),
            (false, "completed", 1, Some("done"))
        );
    }

    #[test]
    fn track_records_failure_interruption_and_heartbeat() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.json");
        let r: Result<(), String> = track(&path, "rebuild", None, |t| {
            t.source_started("s");
            std::thread::sleep(MIN_WRITE_INTERVAL + Duration::from_millis(50));
            heartbeat();
            let p = read_progress(&path).unwrap();
            assert!(p.source("s").unwrap().heartbeat_at > p.started_at);
            Err("boom".into())
        });
        assert!(r.is_err());
        let p = read_progress(&path).unwrap();
        assert_eq!(
            (p.outcome.as_str(), p.error.as_deref()),
            ("failed", Some("boom"))
        );
        assert_eq!(p.source("s").unwrap().outcome.as_deref(), Some("failed"));
        heartbeat(); // no active run: no-op
        let r: Result<(), String> =
            track(&path, "index", None, |_| Err("interrupted by user".into()));
        assert!(r.is_err());
        assert_eq!(read_progress(&path).unwrap().outcome, "interrupted");
    }

    #[test]
    fn ticker_keeps_an_idle_stage_alive_without_progress() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.json");
        let t = ProgressTracker::start(&path, "index", None);
        t.source_started("s");
        let first = read_progress(&path).unwrap().source("s").unwrap().clone();
        // Wait for a tick (bounded: slow CI runners can lag the interval).
        let deadline = Instant::now() + HEARTBEAT_INTERVAL * 4;
        while Instant::now() < deadline
            && read_progress(&path)
                .unwrap()
                .source("s")
                .unwrap()
                .heartbeat_at
                == first.heartbeat_at
        {
            std::thread::sleep(Duration::from_millis(200));
        }
        let later = read_progress(&path).unwrap().source("s").unwrap().clone();
        assert!(later.heartbeat_at > first.heartbeat_at);
        assert_eq!(later.last_progress_at, first.last_progress_at);
        assert_eq!(later.processed, 0);
        t.finish(None);
    }
}
