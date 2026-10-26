//! The background indexing daemon.
//!
//! Up to `workers` threads (one [`PassRunner`] each, `max_parallel_sources`
//! in production) run passes from the fair [`Scheduler`] queue. A second
//! thread enqueues every enabled source each `reconciliation_interval`.
//! Triggers for one source coalesce: at most one queued pass, plus at most
//! one follow-up while a pass runs, so one source never runs twice at a
//! time. Small watcher-targeted passes go first, but aging bounds how long
//! any queued source waits (see [`scheduler::Policy`]). Every pass takes
//! its source's `locks/index-<source_id>.lock` (operation `daemon`); on
//! contention it backs off and re-enqueues itself with reason
//! `lock_contention`. Each worker owns its own database connections (its
//! [`PassRunner`]). The daemon's [`Catalog`] connection is used only to
//! read sources.
//!
//! Relationship graphs are a separate, later stage: a source whose pass
//! published is remembered as graph-pending, and graph work
//! ([`PassRunner::run_graph`]) is only dispatched once no index pass is
//! queued or running (the same barrier as `docmonk index`). A pass that
//! changes nothing still lets a missing, failed or outdated graph be
//! rebuilt without reindexing.

pub mod health;
pub mod pid;
pub mod scheduler;

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use docmonk_core::errors::{DocMonkError, ErrorKind};
use docmonk_core::models::SourceType;
use docmonk_core::paths::Home;
use docmonk_storage::control::{ControlPlane, SourceRecord};
use docmonk_storage::StorageLayout;

use crate::coordinator::{run_source_with, Options, Progress, Registry, SourceResult};
use crate::lock::RunLock;
use crate::progress::now_iso;
use health::{DaemonHealth, SourceWatchStatus};
use scheduler::Scheduler;

use crate::watcher::git::GitWatcher;
use crate::watcher::local::LocalWatcher;
use crate::watcher::network::{NetworkSpec, NetworkWatcher};

/// A batch of touched paths larger than this runs as a full pass.
pub const MAX_TARGETED_PATHS: usize = 200;
/// Reasons that always force a full scan. `watcher_rescan`: the watcher
/// reported an error or that it lost events (a queue overflow), so
/// touched paths no longer describe every change. `git_poll`: a git
/// source's branch moved (ADR 0043).
pub const FORCE_FULL_REASONS: [&str; 4] =
    ["startup", "reconciliation", "watcher_rescan", "git_poll"];

/// What the next pass for one source should scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanRequest {
    pub source_id: String,
    /// The first trigger reason of the batch (telemetry only).
    pub reason: String,
    pub full: bool,
    /// Empty when `full`.
    pub changed_paths: BTreeSet<PathBuf>,
}

/// Source reads for the daemon's own bookkeeping.
pub trait Catalog: Send {
    fn list_sources(&mut self, enabled_only: bool) -> Result<Vec<SourceRecord>, DocMonkError>;
    fn get_source(&mut self, id: &str) -> Result<Option<SourceRecord>, DocMonkError>;
}

fn db_err(e: impl std::fmt::Display) -> DocMonkError {
    DocMonkError::new(ErrorKind::Database, e.to_string())
}

impl Catalog for ControlPlane {
    fn list_sources(&mut self, enabled_only: bool) -> Result<Vec<SourceRecord>, DocMonkError> {
        ControlPlane::list_sources(self, enabled_only).map_err(db_err)
    }
    fn get_source(&mut self, id: &str) -> Result<Option<SourceRecord>, DocMonkError> {
        ControlPlane::get_source(self, id).map_err(db_err)
    }
}

/// Runs one pass. Lives on the worker thread only, so the connections it
/// owns are never shared.
pub trait PassRunner: Send {
    fn run_pass(
        &mut self,
        source: &SourceRecord,
        request: &ScanRequest,
        progress: &mut dyn Progress,
    ) -> Result<SourceResult, DocMonkError>;

    /// The relationship graph stage of one source whose base index is
    /// published (the caller holds the source's lock). Returns the graph
    /// state for the log. Default: no graph stage.
    fn run_graph(&mut self, _source: &SourceRecord) -> Option<String> {
        None
    }
}

/// A graph stage hook for [`CoordinatorRunner`] (the graph lives in crates
/// above this one).
pub type GraphHook = Arc<dyn Fn(&SourceRecord) -> Option<String> + Send + Sync>;

/// The production runner: the coordinator over a worker-owned control
/// plane. A request with touched paths runs as a targeted pass. Anything
/// else is a full scan and diff.
pub struct CoordinatorRunner {
    pub layout: StorageLayout,
    pub control: ControlPlane,
    pub registry: Registry,
    pub opts: Options,
    /// The relationship graph stage, if any.
    pub graph: Option<GraphHook>,
}

impl PassRunner for CoordinatorRunner {
    fn run_pass(
        &mut self,
        source: &SourceRecord,
        request: &ScanRequest,
        progress: &mut dyn Progress,
    ) -> Result<SourceResult, DocMonkError> {
        let targets = (!request.full).then_some(&request.changed_paths);
        run_source_with(
            &self.layout,
            &mut self.control,
            source,
            &self.registry,
            &self.opts,
            targets,
            progress,
        )
    }

    fn run_graph(&mut self, source: &SourceRecord) -> Option<String> {
        self.graph.as_ref().and_then(|g| g(source))
    }
}

#[derive(Debug, Clone)]
pub struct DaemonOptions {
    /// Fairness and path bounds of the queue.
    pub policy: scheduler::Policy,
    pub reconciliation_interval: Duration,
    /// How long one pass waits for `index.lock`.
    pub lock_timeout: Duration,
    pub contention_backoff: Duration,
    /// Attach watchers (`indexing.watch`). Off means reconciliation only.
    pub watch: bool,
    pub debounce_ms: i64,
    /// Network roots are fingerprinted this often.
    pub network_poll: Duration,
    /// Git sources ask their remote for new commits this often.
    pub git_poll: Duration,
    /// Paces the local polling fallback.
    pub local_poll: Duration,
    pub follow_symlinks: bool,
}

impl Default for DaemonOptions {
    /// Reference defaults, with watching off.
    fn default() -> Self {
        Self {
            policy: scheduler::Policy::default(),
            reconciliation_interval: Duration::from_secs(900),
            lock_timeout: Duration::from_secs(30),
            contention_backoff: Duration::from_secs(5),
            watch: false,
            debounce_ms: 2000,
            network_poll: Duration::from_secs(30),
            git_poll: Duration::from_secs(300),
            local_poll: Duration::from_secs(2),
            follow_symlinks: false,
        }
    }
}

impl DaemonOptions {
    pub fn from_config(cfg: &docmonk_config::DocMonkConfig) -> Self {
        Self {
            policy: scheduler::Policy {
                urgent_first: true,
                max_wait: Duration::from_secs(cfg.indexing.fairness_max_wait_seconds.max(1) as u64),
                max_paths: cfg.indexing.max_targeted_paths.max(1) as usize,
            },
            reconciliation_interval: Duration::from_secs(
                cfg.indexing.reconciliation_interval_seconds.max(1) as u64,
            ),
            lock_timeout: Duration::from_secs_f64(cfg.indexing.lock_timeout_seconds),
            contention_backoff: Duration::from_secs(5),
            watch: cfg.indexing.watch,
            debounce_ms: cfg.indexing.debounce_ms,
            network_poll: Duration::from_secs(cfg.indexing.network_poll_seconds.max(1) as u64),
            git_poll: Duration::from_secs(cfg.indexing.git_poll_seconds.max(1) as u64),
            local_poll: Duration::from_secs(2),
            follow_symlinks: cfg.indexing.follow_symlinks,
        }
    }
}

#[derive(Default)]
struct Watchers {
    local: HashMap<String, LocalWatcher>,
    network: HashMap<String, NetworkWatcher>,
    git: HashMap<String, GitWatcher>,
}

#[derive(Default)]
struct State {
    stopped: bool,
    sched: Scheduler,
    online: HashMap<String, bool>,
    last_pass_at: HashMap<String, String>,
    last_reconciliation_at: Option<String>,
    passes: u64,
    /// Sources whose base index was published and whose graph stage has
    /// not run since.
    graph_pending: BTreeSet<String>,
}

/// The progress snapshot shared by concurrently running passes.
#[derive(Default)]
struct RunProgress {
    tracker: Option<crate::progress::ProgressTracker>,
    active: usize,
}

struct Shared {
    home: Home,
    opts: DaemonOptions,
    progress: Mutex<RunProgress>,
    started_at: String,
    catalog: Mutex<Box<dyn Catalog>>,
    state: Mutex<State>,
    cv: Condvar,
    watchers: Mutex<Watchers>,
    /// For watcher callbacks. Weak, so watchers never keep the daemon alive.
    me: Weak<Shared>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// Waits up to `d` or until stopped; true when stopped.
    fn wait_stopped(&self, d: Duration) -> bool {
        let g = self.state();
        let (g, _) = self
            .cv
            .wait_timeout_while(g, d, |s| !s.stopped)
            .unwrap_or_else(|p| p.into_inner());
        g.stopped
    }

    fn enqueue(&self, source_id: &str, reason: &str, paths: &[PathBuf]) {
        {
            let mut s = self.state();
            if s.stopped {
                return;
            }
            if !s.sched.enqueue(source_id, reason, paths) {
                return;
            }
        }
        tracing::info!(
            component = "daemon",
            event = "source_pass_queued",
            source_id,
            reason
        );
        self.cv.notify_all();
    }

    /// Starts the watcher for `source`, if watching is on. Failure (for
    /// example a missing root) leaves the source unwatched until a later
    /// reconciliation retries it.
    fn attach_watcher(&self, source: &SourceRecord) {
        if !self.opts.watch || self.state().stopped {
            return;
        }
        let id = source.id.clone();
        let me = self.me.clone();
        let root = PathBuf::from(&source.path);
        if let Some(remote) = crate::git::remote_dir_of(&root) {
            // The checkout changes only when a pass fetches it, so it gets
            // no filesystem watcher: the remote is polled instead.
            if lock(&self.watchers).git.contains_key(&id) {
                return;
            }
            let sid = id.clone();
            match GitWatcher::start(
                remote,
                self.opts.git_poll,
                Arc::new(move || {
                    if let Some(s) = me.upgrade() {
                        s.enqueue(&sid, "git_poll", &[]);
                    }
                }),
            ) {
                Ok(w) => {
                    lock(&self.watchers).git.insert(id, w);
                }
                Err(e) => {
                    tracing::warn!(component = "daemon", event = "git_watcher_attach_failed", source_id = %id, error = %e);
                }
            }
            return;
        }
        if source.source_type == SourceType::Network {
            if lock(&self.watchers).network.contains_key(&id) {
                return;
            }
            let sid = id.clone();
            let spec = NetworkSpec {
                root,
                include: source.include_patterns.clone(),
                exclude: source.exclude_patterns.clone(),
                follow_symlinks: self.opts.follow_symlinks,
                interval: self.opts.network_poll,
            };
            match NetworkWatcher::start(
                spec,
                Arc::new(move |paths| {
                    if let Some(s) = me.upgrade() {
                        s.enqueue(&sid, "network_watcher", &paths);
                    }
                }),
            ) {
                Ok(w) => {
                    lock(&self.watchers).network.insert(id, w);
                }
                Err(e) => {
                    tracing::warn!(component = "daemon", event = "network_watcher_attach_failed", source_id = %id, error = %e);
                }
            }
            return;
        }
        if lock(&self.watchers).local.contains_key(&id) {
            return;
        }
        let sid = id.clone();
        let (rescan_me, rescan_id) = (me.clone(), id.clone());
        match LocalWatcher::start(
            &root,
            self.opts.debounce_ms,
            self.opts.local_poll,
            false,
            Arc::new(move |path| {
                if let Some(s) = me.upgrade() {
                    s.enqueue(&sid, "local_watcher", &[path]);
                }
            }),
            Arc::new(move |why| {
                tracing::warn!(component = "daemon", event = "watcher_rescan", source_id = %rescan_id, reason = %why);
                if let Some(s) = rescan_me.upgrade() {
                    s.enqueue(&rescan_id, "watcher_rescan", &[]);
                }
            }),
        ) {
            Ok(w) => {
                lock(&self.watchers).local.insert(id, w);
            }
            Err(e) => {
                tracing::warn!(component = "daemon", event = "local_watcher_attach_failed", source_id = %id, error = %e);
            }
        }
    }

    fn reconcile(&self) {
        if self.state().stopped {
            return;
        }
        let sources = lock(&self.catalog).list_sources(true);
        match sources {
            Ok(sources) => {
                for s in &sources {
                    // Re-attaches a watcher whose root was unavailable.
                    self.attach_watcher(s);
                    self.enqueue(&s.id, "reconciliation", &[]);
                }
            }
            Err(e) => {
                tracing::error!(component = "daemon", event = "reconciliation_failed", error = %e.message());
            }
        }
        self.state().last_reconciliation_at = Some(now_iso());
        self.write_health();
    }

    fn build_request(&self, source_id: &str) -> ScanRequest {
        self.state().sched.take_request(source_id)
    }

    fn settle(&self, source_id: &str) {
        let requeued = self.state().sched.settle(source_id);
        if requeued {
            self.cv.notify_all();
        }
    }

    fn run_pass(&self, source_id: &str, runner: &mut dyn PassRunner) {
        let source = match lock(&self.catalog).get_source(source_id) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(component = "daemon", event = "daemon_pass_error", source_id, error = %e.message());
                return;
            }
        };
        // Removed or disabled since it was enqueued.
        let Some(source) = source.filter(|s| s.enabled) else {
            return;
        };
        let lock_ = match RunLock::acquire(
            &crate::lock::source_lock_path(&self.home, source_id),
            "daemon",
            Some(source_id),
            self.opts.lock_timeout,
        ) {
            Ok(l) => l,
            Err(e) if e.kind() == ErrorKind::RunLockTimeout => {
                // Contention, not failure: nothing was consumed yet. This
                // source is "running", so the re-enqueue is a follow-up.
                tracing::warn!(component = "daemon", event = "daemon_pass_lock_contention", source_id, detail = %e.message());
                if !self.wait_stopped(self.opts.contention_backoff) {
                    self.enqueue(source_id, "lock_contention", &[]);
                }
                return;
            }
            Err(e) => {
                tracing::error!(component = "daemon", event = "daemon_pass_error", source_id, error = %e.message());
                return;
            }
        };
        let request = self.build_request(source_id);
        let mut tracker = self.progress_begin(source_id);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runner.run_pass(&source, &request, &mut tracker)
        }))
        .unwrap_or_else(|_| {
            Err(DocMonkError::new(
                ErrorKind::Generic,
                "daemon pass panicked; its pending build was discarded",
            ))
        });
        self.progress_end(source_id, &tracker, result.as_ref().err());
        drop(lock_);
        match result {
            Ok(r) => {
                {
                    let mut s = self.state();
                    s.last_pass_at.insert(source_id.into(), now_iso());
                    s.online.insert(source_id.into(), r.offline.is_none());
                    s.passes += 1;
                    if r.offline.is_none() && r.build_id.is_some() {
                        s.graph_pending.insert(source_id.into());
                    }
                }
                tracing::info!(
                    component = "daemon",
                    event = "daemon_pass_completed",
                    source_id,
                    reason = %request.reason,
                    full = request.full,
                    offline = r.offline.is_some(),
                    indexed = r.indexed,
                    failed = r.failed,
                );
                self.write_health();
            }
            Err(e) => {
                self.state().passes += 1;
                tracing::error!(component = "daemon", event = "daemon_pass_error", source_id, error = %e.message());
            }
        }
    }

    /// Joins (or starts) the shared progress snapshot for one pass.
    fn progress_begin(&self, source_id: &str) -> crate::progress::ProgressTracker {
        let mut p = lock(&self.progress);
        let tracker = p
            .tracker
            .get_or_insert_with(|| {
                crate::progress::ProgressTracker::start(self.home.index_progress(), "daemon", None)
            })
            .clone();
        p.active += 1;
        tracker.source_started(source_id);
        tracker
    }

    /// Leaves the shared snapshot; the last pass out writes the final one.
    fn progress_end(
        &self,
        source_id: &str,
        tracker: &crate::progress::ProgressTracker,
        error: Option<&DocMonkError>,
    ) {
        tracker.source_finished(source_id, error.is_none());
        let mut p = lock(&self.progress);
        p.active = p.active.saturating_sub(1);
        if p.active == 0 {
            if let Some(t) = p.tracker.take() {
                t.finish(error.map(DocMonkError::message));
            }
        }
    }

    fn worker_loop(&self, runner: &mut dyn PassRunner) {
        loop {
            let next = {
                let g = self.state();
                let mut g = self
                    .cv
                    .wait_while(g, |s| !s.stopped && !s.sched.has_queued())
                    .unwrap_or_else(|p| p.into_inner());
                if g.stopped {
                    g.sched.clear_queue();
                    return;
                }
                g.sched.start_next()
            };
            if let Some(id) = next {
                self.run_pass(&id, runner);
                self.settle(&id);
                self.run_graphs(runner);
            }
        }
    }

    /// The barrier: graph work is taken only when no index pass is queued
    /// or running, and only by one worker at a time.
    fn take_graph_batch(&self) -> Vec<String> {
        let mut s = self.state();
        if s.stopped || s.sched.has_queued() || s.sched.running() > 0 {
            return Vec::new();
        }
        std::mem::take(&mut s.graph_pending).into_iter().collect()
    }

    fn run_graphs(&self, runner: &mut dyn PassRunner) {
        for source_id in self.take_graph_batch() {
            let source = match lock(&self.catalog).get_source(&source_id) {
                Ok(Some(s)) if s.enabled => s,
                _ => continue,
            };
            let lock_ = match RunLock::acquire(
                &crate::lock::source_lock_path(&self.home, &source_id),
                "daemon",
                Some(&source_id),
                self.opts.lock_timeout,
            ) {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(component = "daemon", event = "daemon_graph_lock_contention", source_id = %source_id, detail = %e.message());
                    self.state().graph_pending.insert(source_id);
                    continue;
                }
            };
            let state = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                runner.run_graph(&source)
            }))
            .unwrap_or_else(|_| Some("failed".into()));
            drop(lock_);
            if let Some(state) = state {
                tracing::info!(
                    component = "daemon",
                    event = "daemon_graph_completed",
                    source_id = %source_id,
                    state = %state,
                );
            }
        }
    }

    fn snapshot(&self) -> DaemonHealth {
        let sources = lock(&self.catalog).list_sources(false).unwrap_or_default();
        let s = self.state();
        DaemonHealth {
            started_at: self.started_at.clone(),
            updated_at: now_iso(),
            last_reconciliation_at: s.last_reconciliation_at.clone(),
            sources: sources
                .iter()
                .map(|src| SourceWatchStatus {
                    source_id: src.id.clone(),
                    path: src.path.clone(),
                    source_type: src.source_type.as_str().into(),
                    online: s
                        .online
                        .get(&src.id)
                        .copied()
                        .unwrap_or_else(|| std::path::Path::new(&src.path).exists()),
                    last_pass_at: s.last_pass_at.get(&src.id).cloned(),
                })
                .collect(),
        }
    }

    fn write_health(&self) {
        if let Err(e) = health::write_health(&self.home, &self.snapshot()) {
            tracing::warn!(component = "daemon", event = "health_write_failed", error = %e);
        }
    }
}

/// A started daemon. [`Daemon::stop`] (or drop) shuts it down gracefully.
pub struct Daemon {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
    reconciler: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Daemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Daemon").finish_non_exhaustive()
    }
}

impl Daemon {
    /// Starts the worker and reconciliation threads, enqueues one full
    /// `startup` pass per enabled source (anything changed while stopped
    /// is picked up at once), and writes the first health snapshot.
    pub fn start(
        home: Home,
        opts: DaemonOptions,
        catalog: Box<dyn Catalog>,
        runner: Box<dyn PassRunner>,
    ) -> Result<Self, DocMonkError> {
        Self::start_with_runners(home, opts, catalog, vec![runner])
    }

    /// [`Self::start`] with one worker thread per runner: up to
    /// `runners.len()` different sources index at the same time.
    pub fn start_with_runners(
        home: Home,
        opts: DaemonOptions,
        catalog: Box<dyn Catalog>,
        runners: Vec<Box<dyn PassRunner>>,
    ) -> Result<Self, DocMonkError> {
        if runners.is_empty() {
            return Err(DocMonkError::usage("daemon needs at least one pass runner"));
        }
        let opts_policy = opts.policy;
        let shared = Arc::new_cyclic(|me| Shared {
            home,
            opts,
            started_at: now_iso(),
            catalog: Mutex::new(catalog),
            progress: Mutex::new(RunProgress::default()),
            state: Mutex::new(State {
                sched: Scheduler::with_policy(opts_policy),
                ..State::default()
            }),
            cv: Condvar::new(),
            watchers: Mutex::new(Watchers::default()),
            me: me.clone(),
        });
        let sources = lock(&shared.catalog).list_sources(true)?;
        for s in &sources {
            shared.attach_watcher(s);
        }
        let spawn_err = |e: std::io::Error| DocMonkError::new(ErrorKind::Generic, e.to_string());
        let mut workers = Vec::with_capacity(runners.len());
        for (i, runner) in runners.into_iter().enumerate() {
            let w = Arc::clone(&shared);
            workers.push(
                std::thread::Builder::new()
                    .name(format!("docmonk-daemon-worker-{i}"))
                    .spawn(move || {
                        let mut runner = runner;
                        w.worker_loop(runner.as_mut());
                    })
                    .map_err(spawn_err)?,
            );
        }
        let r = Arc::clone(&shared);
        let reconciler = std::thread::Builder::new()
            .name("docmonk-daemon-reconcile".into())
            .spawn(move || {
                while !r.wait_stopped(r.opts.reconciliation_interval) {
                    r.reconcile();
                }
            })
            .map_err(spawn_err)?;
        for s in &sources {
            shared.enqueue(&s.id, "startup", &[]);
        }
        shared.write_health();
        tracing::info!(
            component = "daemon",
            event = "daemon_started",
            sources = sources.len()
        );
        Ok(Self {
            shared,
            workers,
            reconciler: Some(reconciler),
        })
    }

    /// Enqueues a pass for `source_id` (coalesced). `paths` are touched
    /// paths for a targeted pass. Empty means a full pass.
    pub fn enqueue_source(&self, source_id: &str, reason: &str, paths: &[PathBuf]) {
        self.shared.enqueue(source_id, reason, paths);
    }

    /// Fault injection (tests/drills only): delivers a watcher error to
    /// `source_id`'s local watcher, as the OS would after losing events.
    /// False when the source has no local watcher.
    pub fn inject_watcher_error(&self, source_id: &str) -> bool {
        match lock(&self.shared.watchers).local.get(source_id) {
            Some(w) => {
                w.inject(Err(notify::Error::generic("injected watcher error")));
                true
            }
            None => false,
        }
    }

    /// Enqueues every enabled source for a full pass now.
    pub fn reconcile_now(&self) {
        self.shared.reconcile();
    }

    /// Passes finished so far (succeeded or failed).
    pub fn passes_completed(&self) -> u64 {
        self.shared.state().passes
    }

    /// Passes running right now.
    pub fn running(&self) -> usize {
        self.shared.state().sched.running()
    }

    /// True when nothing is queued or running.
    pub fn is_idle(&self) -> bool {
        self.shared.state().sched.is_idle()
    }

    /// Waits until idle, up to `timeout`; true when idle.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.is_idle() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.is_idle()
    }

    /// Graceful shutdown. New triggers are refused, and queued passes are
    /// dropped. A pass already running finishes, because it commits per
    /// file anyway. Then both threads are joined and a final health
    /// snapshot is written.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.shared.state().stopped = true;
        self.shared.cv.notify_all();
        // Watchers first: debounced paths not yet due are dropped, and the
        // next start's startup pass covers them.
        let watchers = std::mem::take(&mut *lock(&self.shared.watchers));
        for w in watchers.local.values() {
            w.flush();
        }
        drop(watchers);
        for h in std::mem::take(&mut self.workers)
            .into_iter()
            .chain(self.reconciler.take())
        {
            let _ = h.join();
        }
        self.shared.write_health();
        tracing::info!(component = "daemon", event = "daemon_stopped");
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if !self.workers.is_empty() || self.reconciler.is_some() {
            self.shutdown();
        }
    }
}

/// `daemon status` uptime text (`cli/daemon._format_uptime`).
pub fn format_uptime(seconds: f64) -> String {
    let seconds = seconds as i64;
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let (minutes, secs) = (seconds.div_euclid(60), seconds.rem_euclid(60));
    if minutes < 60 {
        return format!("{minutes}m {secs}s");
    }
    let (hours, minutes) = (minutes.div_euclid(60), minutes.rem_euclid(60));
    if hours < 24 {
        return format!("{hours}h {minutes}m");
    }
    format!("{}d {}h", hours.div_euclid(24), hours.rem_euclid(24))
}

/// The `daemon status --json` payload (`cli/daemon.status`).
pub fn status_payload(home: &Home) -> serde_json::Value {
    let info = pid::running_daemon(home);
    let snapshot = health::read_health(home);
    let uptime = info.as_ref().and_then(|i| {
        let started = chrono::DateTime::parse_from_rfc3339(&i.started_at).ok()?;
        Some((chrono::Utc::now() - started.with_timezone(&chrono::Utc)).as_seconds_f64())
    });
    serde_json::json!({
        "running": info.is_some(),
        "pid": info.as_ref().map(|i| i.pid),
        "started_at": info.as_ref().map(|i| i.started_at.clone()),
        "uptime_seconds": uptime,
        "last_reconciliation_at": snapshot.as_ref().and_then(|s| s.last_reconciliation_at.clone()),
        "sources": snapshot.map(|s| s.sources).unwrap_or_default(),
    })
}
