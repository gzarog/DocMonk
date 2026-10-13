//! Daemon worker semantics against a scripted runner: coalescing,
//! follow-ups, fairness across sources, lock contention, reconciliation
//! and the health snapshot.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use docmonk_core::errors::DocMonkError;
use docmonk_core::models::SourceType;
use docmonk_core::paths::Home;
use docmonk_indexing::coordinator::{Progress, SourceResult};
use docmonk_indexing::daemon::{
    format_uptime, health, Catalog, Daemon, DaemonOptions, PassRunner, ScanRequest,
};
use docmonk_indexing::lock::RunLock;
use docmonk_storage::control::SourceRecord;

fn source(id: &str, path: &str) -> SourceRecord {
    SourceRecord {
        id: id.into(),
        path: path.into(),
        source_type: SourceType::Local,
        enabled: true,
        include_patterns: vec![],
        exclude_patterns: vec![],
        created_at: String::new(),
        updated_at: String::new(),
    }
}

#[derive(Clone)]
struct Fake(Arc<Mutex<Vec<SourceRecord>>>);
impl Catalog for Fake {
    fn list_sources(&mut self, enabled_only: bool) -> Result<Vec<SourceRecord>, DocMonkError> {
        let v = self.0.lock().unwrap().clone();
        Ok(v.into_iter()
            .filter(|s| !enabled_only || s.enabled)
            .collect())
    }
    fn get_source(&mut self, id: &str) -> Result<Option<SourceRecord>, DocMonkError> {
        Ok(self.0.lock().unwrap().iter().find(|s| s.id == id).cloned())
    }
}

type Log = Arc<Mutex<Vec<ScanRequest>>>;

/// Records each request. While `gate` holds a value, passes block until
/// it is taken, so the test controls what happens mid-pass.
struct Scripted {
    log: Log,
    gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
    offline: bool,
}
impl PassRunner for Scripted {
    fn run_pass(
        &mut self,
        source: &SourceRecord,
        request: &ScanRequest,
        _p: &mut dyn Progress,
    ) -> Result<SourceResult, DocMonkError> {
        self.log.lock().unwrap().push(request.clone());
        let (m, cv) = &*self.gate;
        let _g = cv.wait_while(m.lock().unwrap(), |closed| *closed).unwrap();
        Ok(SourceResult {
            source_id: source.id.clone(),
            offline: self.offline.then(|| "gone".into()),
            ..SourceResult::default()
        })
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    home: Home,
    log: Log,
    gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
    daemon: Option<Daemon>,
}

impl Harness {
    fn new(ids: &[&str], closed: bool, opts: DaemonOptions, offline: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path());
        std::fs::create_dir_all(home.locks_dir()).unwrap();
        let cat = Fake(Arc::new(Mutex::new(
            ids.iter()
                .map(|i| source(i, &dir.path().display().to_string()))
                .collect(),
        )));
        let log: Log = Arc::default();
        let gate = Arc::new((Mutex::new(closed), std::sync::Condvar::new()));
        let runner = Scripted {
            log: log.clone(),
            gate: gate.clone(),
            offline,
        };
        let daemon = Daemon::start(home.clone(), opts, Box::new(cat), Box::new(runner)).unwrap();
        Self {
            _dir: dir,
            home,
            log,
            gate,
            daemon: Some(daemon),
        }
    }
    fn d(&self) -> &Daemon {
        self.daemon.as_ref().unwrap()
    }
    fn open(&self) {
        *self.gate.0.lock().unwrap() = false;
        self.gate.1.notify_all();
    }
    fn wait_log(&self, n: usize) {
        for _ in 0..500 {
            if self.log.lock().unwrap().len() >= n {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("expected {n} passes, got {:?}", self.log.lock().unwrap());
    }
    fn order(&self) -> Vec<(String, String, bool)> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .map(|r| (r.source_id.clone(), r.reason.clone(), r.full))
            .collect()
    }
}

fn opts() -> DaemonOptions {
    DaemonOptions {
        reconciliation_interval: Duration::from_secs(3600),
        lock_timeout: Duration::from_millis(100),
        contention_backoff: Duration::from_millis(50),
        ..DaemonOptions::default()
    }
}

fn t(id: &str, reason: &str, full: bool) -> (String, String, bool) {
    (id.into(), reason.into(), full)
}

#[test]
fn startup_runs_one_full_pass_per_source_and_bursts_coalesce() {
    let h = Harness::new(&["a", "b"], true, opts(), false);
    h.wait_log(1); // "a" is running and blocked
                   // Burst while "a" runs: one follow-up for "a"; "b" is already queued.
    for _ in 0..5 {
        h.d()
            .enqueue_source("a", "local_watcher", &[PathBuf::from("x.py")]);
        h.d()
            .enqueue_source("b", "local_watcher", &[PathBuf::from("y.py")]);
    }
    h.open();
    assert!(h.d().wait_idle(Duration::from_secs(5)));
    // Fair: the follow-up for "a" goes behind "b" in the queue.
    assert_eq!(
        h.order(),
        vec![
            t("a", "startup", true),
            t("b", "startup", true),
            t("a", "local_watcher", false),
        ]
    );
    let last = h.log.lock().unwrap()[2].clone();
    assert_eq!(last.changed_paths.len(), 1);
    assert_eq!(h.d().passes_completed(), 3);
}

#[test]
fn too_many_touched_paths_or_forced_reason_means_full() {
    let h = Harness::new(&["a"], false, opts(), false);
    assert!(h.d().wait_idle(Duration::from_secs(5)));
    let many: Vec<PathBuf> = (0..201).map(|i| PathBuf::from(format!("f{i}"))).collect();
    h.d().enqueue_source("a", "local_watcher", &many);
    assert!(h.d().wait_idle(Duration::from_secs(5)));
    h.d()
        .enqueue_source("a", "network_poll", &[PathBuf::from("one")]);
    assert!(h.d().wait_idle(Duration::from_secs(5)));
    h.d().reconcile_now();
    assert!(h.d().wait_idle(Duration::from_secs(5)));
    assert_eq!(
        h.order(),
        vec![
            t("a", "startup", true),
            t("a", "local_watcher", true),
            t("a", "network_poll", false),
            t("a", "reconciliation", true),
        ]
    );
}

#[test]
fn lock_contention_backs_off_and_retries() {
    let h = Harness::new(&["a"], true, opts(), false);
    h.wait_log(1);
    // Daemon passes hold their source's own lock (ADR 0033).
    let held = RunLock::acquire(
        &docmonk_indexing::lock::source_lock_path(&h.home, "a"),
        "index",
        None,
        Duration::from_millis(200),
    );
    // The daemon's pass holds the lock, so we cannot get it yet.
    assert!(held.is_err());
    h.open();
    assert!(h.d().wait_idle(Duration::from_secs(5)));
    let held = RunLock::acquire(
        &docmonk_indexing::lock::source_lock_path(&h.home, "a"),
        "index",
        None,
        Duration::from_secs(5),
    )
    .unwrap();
    h.d()
        .enqueue_source("a", "local_watcher", &[PathBuf::from("x")]);
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(h.log.lock().unwrap().len(), 1, "no pass while locked");
    drop(held);
    h.wait_log(2);
    assert!(h.d().wait_idle(Duration::from_secs(5)));
    // The touched path survived the contention: nothing was consumed.
    assert_eq!(h.order()[1], t("a", "local_watcher", false));
}

#[test]
fn reconciliation_timer_and_health_snapshot() {
    let mut o = opts();
    o.reconciliation_interval = Duration::from_millis(150);
    let mut h = Harness::new(&["a"], false, o, true);
    h.wait_log(3);
    let snap = health::read_health(&h.home).unwrap();
    assert!(snap.last_reconciliation_at.is_some());
    assert_eq!(snap.sources.len(), 1);
    assert_eq!(snap.sources[0].source_id, "a");
    assert_eq!(snap.sources[0].source_type, "local");
    assert!(!snap.sources[0].online, "offline pass result");
    assert!(snap.sources[0].last_pass_at.is_some());
    assert!(h.order().iter().skip(1).all(|r| r.1 == "reconciliation"));
    h.daemon.take().unwrap().stop();
    let n = h.log.lock().unwrap().len();
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(h.log.lock().unwrap().len(), n, "no passes after stop");
}

#[test]
fn stop_lets_the_running_pass_finish_and_drops_queued_ones() {
    let mut h = Harness::new(&["a", "b"], true, opts(), false);
    h.wait_log(1);
    let gate = h.gate.clone();
    let opener = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        *gate.0.lock().unwrap() = false;
        gate.1.notify_all();
    });
    h.daemon.take().unwrap().stop();
    opener.join().unwrap();
    assert_eq!(h.order(), vec![t("a", "startup", true)]);
    assert!(health::read_health(&h.home).unwrap().sources[0]
        .last_pass_at
        .is_some());
}

#[test]
fn uptime_format_cases() {
    for (s, want) in [
        (0.0, "0s"),
        (59.9, "59s"),
        (60.0, "1m 0s"),
        (3599.0, "59m 59s"),
        (3600.0, "1h 0m"),
        (86399.0, "23h 59m"),
        (86400.0, "1d 0h"),
        (90061.0, "1d 1h"),
    ] {
        assert_eq!(format_uptime(s), want);
    }
}

mod common;

#[test]
fn coordinator_runner_indexes_and_publishes_progress() {
    use docmonk_indexing::coordinator::{Options, Registry};
    use docmonk_indexing::daemon::CoordinatorRunner;
    use docmonk_storage::StorageLayout;

    let dir = tempfile::tempdir().unwrap();
    let home = common::home(dir.path());
    home.ensure_layout().unwrap();
    let root = dir.path().join("src");
    common::write(&root, "a.py", "def f():\n    return 1\n");
    common::write(&root, "notes.md", "# Notes\n\nhello\n");
    let mut cp = common::control(&home);
    let src = common::add_source(&mut cp, &root, &[], &[]);
    let runner = CoordinatorRunner {
        layout: StorageLayout::new(&home),
        control: common::control(&home),
        registry: Registry::raw(),
        opts: Options::from_config(&docmonk_config::DocMonkConfig::default()),
        graph: None,
    };
    let d = Daemon::start(home.clone(), opts(), Box::new(cp), Box::new(runner)).unwrap();
    for _ in 0..500 {
        if d.passes_completed() >= 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(d.wait_idle(Duration::from_secs(10)));
    d.stop();
    let p = docmonk_indexing::progress::read_progress(&home.index_progress()).unwrap();
    assert!(!p.running);
    assert_eq!(p.operation.as_deref(), Some("daemon"));
    let indexed: i64 = p.sources.iter().map(|s| s.indexed).sum();
    assert_eq!(indexed, 2);
    assert!(p.sources.iter().all(|s| !s.active));
    let snap = health::read_health(&home).unwrap();
    assert_eq!(snap.sources[0].source_id, src.id);
    assert!(snap.sources[0].online);
    assert!(snap.sources[0].last_pass_at.is_some());
}

/// Records each request, then runs the real coordinator.
struct Logged(Log, docmonk_indexing::daemon::CoordinatorRunner);
impl PassRunner for Logged {
    fn run_pass(
        &mut self,
        source: &SourceRecord,
        request: &ScanRequest,
        p: &mut dyn Progress,
    ) -> Result<SourceResult, DocMonkError> {
        self.0.lock().unwrap().push(request.clone());
        self.1.run_pass(source, request, p)
    }
}

fn wait_until(mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..500 {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn watched_daemon(network: bool) -> (tempfile::TempDir, PathBuf, Home, Log, Daemon, SourceRecord) {
    use docmonk_indexing::coordinator::{Options, Registry};
    use docmonk_indexing::daemon::CoordinatorRunner;
    use docmonk_storage::StorageLayout;

    let dir = tempfile::tempdir().unwrap();
    let home = common::home(dir.path());
    home.ensure_layout().unwrap();
    let root = dir.path().join("src");
    common::write(&root, "a.py", "A = 1\n");
    let mut cp = common::control(&home);
    let mut src = common::add_source(&mut cp, &root, &[], &[]);
    if network {
        src.source_type = SourceType::Network;
    }
    let log: Log = Arc::default();
    let runner = Logged(
        log.clone(),
        CoordinatorRunner {
            layout: StorageLayout::new(&home),
            control: common::control(&home),
            registry: Registry::raw(),
            opts: Options::from_config(&docmonk_config::DocMonkConfig::default()),
            graph: None,
        },
    );
    let catalog = Fake(Arc::new(Mutex::new(vec![src.clone()])));
    let o = DaemonOptions {
        watch: true,
        debounce_ms: 50,
        network_poll: Duration::from_millis(100),
        ..opts()
    };
    let d = Daemon::start(home.clone(), o, Box::new(catalog), Box::new(runner)).unwrap();
    assert!(wait_until(|| d.passes_completed() >= 1));
    assert!(d.wait_idle(Duration::from_secs(10)));
    (dir, root, home, log, d, src)
}

fn check_watched(network: bool, reason: &str) {
    let (_dir, root, home, log, d, src) = watched_daemon(network);
    // Let the watcher settle before changing the tree.
    std::thread::sleep(Duration::from_millis(300));
    common::write(&root, "b.py", "B = 2\n");
    assert!(
        wait_until(|| log.lock().unwrap().iter().any(|r| r.reason == reason)),
        "{reason}: {:?}",
        log.lock().unwrap()
    );
    assert!(d.wait_idle(Duration::from_secs(10)));
    d.stop();
    let req = log
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.reason == reason)
        .cloned()
        .unwrap();
    assert!(!req.full, "a watcher pass is targeted");
    assert!(req.changed_paths.iter().any(|p| p.ends_with("b.py")));
    let cp = common::control(&home);
    let active = cp.state(&src.id).unwrap().active_build_id.unwrap();
    let store = docmonk_storage::knowledge::ProjectStore::open(
        &docmonk_storage::StorageLayout::new(&home),
        &docmonk_core::paths::project_id_for_canonical(&src.path),
        &src.id,
        8,
    )
    .unwrap();
    let mut files: Vec<String> = store
        .files(&active)
        .unwrap()
        .into_iter()
        .map(|f| f.rel_path)
        .collect();
    files.sort();
    assert_eq!(files, ["a.py", "b.py"]);
}

#[test]
fn local_watcher_triggers_targeted_pass() {
    check_watched(false, "local_watcher");
}

#[test]
fn network_poller_triggers_targeted_pass() {
    check_watched(true, "network_watcher");
}

/// Logs passes and graph stages; passes publish a build.
struct Staged(Arc<Mutex<Vec<String>>>);
impl PassRunner for Staged {
    fn run_pass(
        &mut self,
        source: &SourceRecord,
        _request: &ScanRequest,
        _p: &mut dyn Progress,
    ) -> Result<SourceResult, DocMonkError> {
        self.0.lock().unwrap().push(format!("pass:{}", source.id));
        std::thread::sleep(Duration::from_millis(20));
        Ok(SourceResult {
            source_id: source.id.clone(),
            build_id: Some(format!("b-{}", source.id)),
            ..SourceResult::default()
        })
    }

    fn run_graph(&mut self, source: &SourceRecord) -> Option<String> {
        self.0.lock().unwrap().push(format!("graph:{}", source.id));
        Some("ready".into())
    }
}

#[test]
fn graph_stages_run_only_after_every_queued_index_pass() {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::new(dir.path());
    std::fs::create_dir_all(home.locks_dir()).unwrap();
    let ids = ["a", "b", "c", "d"];
    let cat = Fake(Arc::new(Mutex::new(
        ids.iter()
            .map(|i| source(i, &dir.path().display().to_string()))
            .collect(),
    )));
    let log: Arc<Mutex<Vec<String>>> = Arc::default();
    let runners: Vec<Box<dyn PassRunner>> = (0..2)
        .map(|_| Box::new(Staged(log.clone())) as Box<dyn PassRunner>)
        .collect();
    let d = Daemon::start_with_runners(home, opts(), Box::new(cat), runners).unwrap();
    for _ in 0..500 {
        if log
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.starts_with("graph:"))
            .count()
            == ids.len()
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    d.stop();
    let log = log.lock().unwrap().clone();
    let last_pass = log.iter().rposition(|e| e.starts_with("pass:")).unwrap();
    let first_graph = log.iter().position(|e| e.starts_with("graph:")).unwrap();
    assert!(last_pass < first_graph, "barrier violated: {log:?}");
    let mut graphs: Vec<&String> = log.iter().filter(|e| e.starts_with("graph:")).collect();
    graphs.sort();
    graphs.dedup();
    assert_eq!(graphs.len(), ids.len(), "{log:?}");
}
