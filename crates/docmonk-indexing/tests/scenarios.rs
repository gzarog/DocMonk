//! Coordinator behavior: publication, warm no-op, incomplete/offline
//! safety, retries, panic isolation, source isolation, locks and scale.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use docmonk_core::paths::project_id_for_canonical;
use docmonk_indexing::coordinator::{
    index_all, run_source, NoProgress, Options, PrepareInput, ProcessError, Processor, Progress,
    ProgressEvent, Registry,
};
use docmonk_indexing::lock::RunLock;
use docmonk_storage::control::BuildState;
use docmonk_storage::knowledge::{FileKnowledge, ProjectStore};
use docmonk_storage::StorageLayout;

fn opts() -> Options {
    let mut o = Options::from_config(&docmonk_config::DocMonkConfig::default());
    o.workers = 4;
    o
}

fn visible_files(
    layout: &StorageLayout,
    cp: &docmonk_storage::control::ControlPlane,
    src: &docmonk_storage::control::SourceRecord,
) -> Vec<(String, String)> {
    let state = cp.state(&src.id).unwrap();
    let Some(active) = state.active_build_id else {
        return vec![];
    };
    let store =
        ProjectStore::open(layout, &project_id_for_canonical(&src.path), &src.id, 8).unwrap();
    store
        .files(&active)
        .unwrap()
        .into_iter()
        .map(|f| (f.rel_path, f.status))
        .collect()
}

#[test]
fn full_then_warm_then_incremental() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("r");
    for i in 0..50 {
        common::write(&root, &format!("pkg/m{i}.py"), &format!("x = {i}\n"));
    }
    let home = common::home(tmp.path());
    let layout = StorageLayout::new(&home);
    let mut cp = common::control(&home);
    let src = common::add_source(&mut cp, &root, &[], &[]);
    let reg = Registry::raw();

    let r = run_source(&layout, &mut cp, &src, &reg, &opts(), &mut NoProgress).unwrap();
    assert!(r.plan.starts_with("full"));
    assert!(r.published);
    assert_eq!((r.counts.new, r.indexed), (50, 50));
    assert_eq!(cp.state(&src.id).unwrap().build_state, BuildState::Ready);

    let warm = run_source(&layout, &mut cp, &src, &reg, &opts(), &mut NoProgress).unwrap();
    assert_eq!(warm.plan, "incremental");
    assert!(!warm.published, "no-op pass creates no build");
    assert_eq!((warm.counts.unchanged, warm.timings.hash_calls), (50, 0));

    common::write(&root, "pkg/m3.py", "x = 'edited'\n");
    std::fs::remove_file(root.join("pkg/m4.py")).unwrap();
    let inc = run_source(&layout, &mut cp, &src, &reg, &opts(), &mut NoProgress).unwrap();
    assert!(inc.published);
    assert_eq!(
        (
            inc.counts.changed,
            inc.counts.deleted,
            inc.carried_forward,
            inc.indexed
        ),
        (1, 1, 48, 1)
    );
    assert_eq!(visible_files(&layout, &cp, &src).len(), 49);
}

#[test]
fn incomplete_scan_keeps_unseen_files_and_offline_root_keeps_build() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("r");
    common::write(&root, "a.py", "a");
    common::write(&root, "sub/b.py", "b");
    common::write(&root, "sub/c.py", "c");
    let home = common::home(tmp.path());
    let layout = StorageLayout::new(&home);
    let mut cp = common::control(&home);
    let src = common::add_source(&mut cp, &root, &[], &[]);
    let reg = Registry::raw();
    run_source(&layout, &mut cp, &src, &reg, &opts(), &mut NoProgress).unwrap();

    common::write(&root, "a.py", "a2");
    let mut o = opts();
    o.inject_unreadable = vec![root.join("sub")];
    let r = run_source(&layout, &mut cp, &src, &reg, &o, &mut NoProgress).unwrap();
    assert_eq!(
        (r.counts.deleted, r.counts.unseen_kept, r.counts.changed),
        (0, 2, 1)
    );
    assert!(!r.scan_errors.is_empty());
    assert_eq!(
        visible_files(&layout, &cp, &src).len(),
        3,
        "unreadable subtree never looks deleted"
    );

    // Offline root: nothing is touched.
    std::fs::rename(&root, tmp.path().join("moved-away")).unwrap();
    let before = cp.state(&src.id).unwrap().active_build_id;
    let off = run_source(&layout, &mut cp, &src, &reg, &opts(), &mut NoProgress).unwrap();
    assert!(off.offline.is_some());
    assert_eq!(cp.state(&src.id).unwrap().active_build_id, before);
    assert_eq!(cp.state(&src.id).unwrap().online_status, "offline");
    assert_eq!(visible_files(&layout, &cp, &src).len(), 3);
}

/// Fails `fail.py` (transient or not) and panics on `panic.py`.
struct Flaky {
    transient: bool,
    calls: AtomicUsize,
}

impl Processor for Flaky {
    fn prepare(&self, input: &PrepareInput) -> Result<FileKnowledge, ProcessError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if input.rel_path == "panic.py" {
            panic!("boom");
        }
        if input.rel_path == "fail.py" {
            return Err(ProcessError {
                code: "E".into(),
                message: "nope".into(),
                transient: self.transient,
            });
        }
        Ok(FileKnowledge::default())
    }
}

#[test]
fn failures_are_isolated_per_file_and_retried_with_backoff() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("r");
    for f in ["ok.py", "fail.py", "panic.py"] {
        common::write(&root, f, f);
    }
    let home = common::home(tmp.path());
    let layout = StorageLayout::new(&home);
    let mut cp = common::control(&home);
    let src = common::add_source(&mut cp, &root, &[], &[]);
    let flaky = Arc::new(Flaky {
        transient: true,
        calls: AtomicUsize::new(0),
    });
    let mut reg = Registry::raw();
    reg.code = flaky.clone();

    let first_done = std::time::Instant::now();
    let r = run_source(&layout, &mut cp, &src, &reg, &opts(), &mut NoProgress).unwrap();
    assert!(r.published, "file failures never block publication");
    assert_eq!((r.indexed, r.retrying, r.failed), (1, 1, 1));
    let files = visible_files(&layout, &cp, &src);
    assert!(files.contains(&("fail.py".into(), "retry".into())));
    assert!(files.contains(&("panic.py".into(), "failed".into())));

    // Not due yet: the retrying file is carried forward, not reprocessed.
    // (Under heavy load the first backoff can already have elapsed; then
    // the file is due and is retried exactly once.)
    let calls = flaky.calls.load(Ordering::SeqCst);
    let warm = run_source(&layout, &mut cp, &src, &reg, &opts(), &mut NoProgress).unwrap();
    let elapsed = first_done.elapsed().as_secs_f64();
    if elapsed < docmonk_indexing::retry::BASE_DELAY_SECONDS {
        assert_eq!(warm.counts.changed, 0);
        assert_eq!(flaky.calls.load(Ordering::SeqCst), calls);
    } else {
        assert!(flaky.calls.load(Ordering::SeqCst) <= calls + 1);
    }
}

#[test]
fn permanent_failures_stop_retrying() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("r");
    common::write(&root, "fail.py", "x");
    let home = common::home(tmp.path());
    let layout = StorageLayout::new(&home);
    let mut cp = common::control(&home);
    let src = common::add_source(&mut cp, &root, &[], &[]);
    let mut reg = Registry::raw();
    reg.code = Arc::new(Flaky {
        transient: false,
        calls: AtomicUsize::new(0),
    });
    let r = run_source(&layout, &mut cp, &src, &reg, &opts(), &mut NoProgress).unwrap();
    assert_eq!((r.failed, r.retrying), (1, 0));
    let warm = run_source(&layout, &mut cp, &src, &reg, &opts(), &mut NoProgress).unwrap();
    assert_eq!(
        warm.counts.changed, 0,
        "failed files wait for a content change"
    );
}

#[test]
fn oversized_files_are_recorded_not_processed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("r");
    common::write(&root, "big.py", &"x".repeat(2048));
    let home = common::home(tmp.path());
    let layout = StorageLayout::new(&home);
    let mut cp = common::control(&home);
    let src = common::add_source(&mut cp, &root, &[], &[]);
    let mut o = opts();
    o.max_file_size_bytes = 1024;
    let r = run_source(
        &layout,
        &mut cp,
        &src,
        &Registry::raw(),
        &o,
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(r.skipped_limit, 1);
    assert_eq!(
        visible_files(&layout, &cp, &src),
        vec![("big.py".into(), "skipped_limit".into())]
    );
}

struct Collect(Vec<ProgressEvent>);
impl Progress for Collect {
    fn event(&mut self, e: &ProgressEvent) {
        self.0.push(e.clone());
    }
}

#[test]
fn one_failing_source_does_not_stop_others_and_lock_is_bounded() {
    let tmp = tempfile::tempdir().unwrap();
    let home = common::home(tmp.path());
    let mut cp = common::control(&home);
    let good1 = tmp.path().join("g1");
    let bad = tmp.path().join("bad");
    let good2 = tmp.path().join("g2");
    common::write(&good1, "a.py", "a");
    common::write(&bad, "b.py", "b");
    common::write(&good2, "c.py", "c");
    common::add_source(&mut cp, &good1, &[], &[]);
    let bad_src = common::add_source(&mut cp, &bad, &[], &[]);
    common::add_source(&mut cp, &good2, &[], &[]);
    // Break the bad source's project store: its path is a file.
    let layout = StorageLayout::new(&home);
    let pdir = layout.project_dir(&project_id_for_canonical(&bad_src.path));
    std::fs::create_dir_all(pdir.parent().unwrap()).unwrap();
    std::fs::write(&pdir, "not a directory").unwrap();

    let mut events = Collect(vec![]);
    let summary = index_all(&home, &mut cp, &Registry::raw(), &opts(), &mut events).unwrap();
    assert_eq!(summary.sources.len(), 2);
    assert_eq!(summary.failures.len(), 1);
    assert_eq!(summary.failures[0].0, bad_src.id);
    assert!(events
        .0
        .iter()
        .any(|e| matches!(e, ProgressEvent::SourceFinished { ok: false, .. })));
    assert!(events
        .0
        .iter()
        .any(|e| matches!(e, ProgressEvent::FileDone { .. })));

    // Lock contention is bounded and diagnosable.
    let held = RunLock::acquire(
        &home.locks_dir().join("index.lock"),
        "daemon",
        None,
        Duration::from_secs(1),
    )
    .unwrap();
    let mut o = opts();
    o.lock_timeout = Duration::from_millis(300);
    let started = Instant::now();
    let err = index_all(&home, &mut cp, &Registry::raw(), &o, &mut NoProgress).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(
        err.message().contains(if cfg!(windows) {
            "owner unknown"
        } else {
            "operation=daemon"
        }),
        "{}",
        err.message()
    );
    drop(held);
}

#[test]
fn many_sources_make_progress_with_bounded_work() {
    let tmp = tempfile::tempdir().unwrap();
    let home = common::home(tmp.path());
    let mut cp = common::control(&home);
    let n_sources = 150;
    for s in 0..n_sources {
        let root = tmp.path().join(format!("repo{s:03}"));
        for f in 0..5 {
            common::write(&root, &format!("src/f{f}.py"), &format!("v = {s}{f}\n"));
        }
        common::add_source(&mut cp, &root, &[], &[]);
    }
    let started = Instant::now();
    let summary = index_all(&home, &mut cp, &Registry::raw(), &opts(), &mut NoProgress).unwrap();
    let cold = started.elapsed();
    assert!(summary.failures.is_empty());
    assert_eq!(summary.sources.len(), n_sources);
    assert!(summary
        .sources
        .iter()
        .all(|s| s.published && s.indexed == 5));
    let started = Instant::now();
    let warm = index_all(&home, &mut cp, &Registry::raw(), &opts(), &mut NoProgress).unwrap();
    let warm_t = started.elapsed();
    assert!(warm
        .sources
        .iter()
        .all(|s| !s.published && s.counts.unchanged == 5));
    eprintln!("150 sources x 5 files: cold {cold:?}, warm {warm_t:?}");
}

/// Fails the build after every file was written (before publication).
struct FailingFinalizer;

impl docmonk_indexing::coordinator::BuildFinalizer for FailingFinalizer {
    fn finalize(
        &self,
        _store: &mut ProjectStore,
        _build_id: &str,
        _touched: &[String],
    ) -> Result<docmonk_indexing::coordinator::FinalizeReport, ProcessError> {
        Err(ProcessError {
            code: "boom".into(),
            message: "finalizer failed".into(),
            transient: false,
        })
    }
}

/// (files visible in the active build, committed hash of the file).
type Seen = Arc<std::sync::Mutex<Vec<(i64, Option<String>)>>>;

/// Records what a separate reader connection sees while a build runs.
struct Observer {
    db: std::path::PathBuf,
    build: String,
    seen: Seen,
}

impl Processor for Observer {
    fn prepare(&self, input: &PrepareInput) -> Result<FileKnowledge, ProcessError> {
        let conn = rusqlite::Connection::open_with_flags(
            &self.db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let files: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE build_id = ?1",
                [&self.build],
                |r| r.get(0),
            )
            .unwrap();
        let hash: Option<String> = conn
            .query_row(
                "SELECT content_hash FROM files WHERE build_id = ?1 AND rel_path = ?2",
                [&self.build, &input.rel_path],
                |r| r.get(0),
            )
            .ok();
        self.seen.lock().unwrap().push((files, hash));
        Ok(FileKnowledge::default())
    }
}

fn snapshot(
    layout: &StorageLayout,
    src: &docmonk_storage::control::SourceRecord,
    build: &str,
) -> Vec<String> {
    let store =
        ProjectStore::open(layout, &project_id_for_canonical(&src.path), &src.id, 8).unwrap();
    let mut rows: Vec<String> = store
        .files(build)
        .unwrap()
        .into_iter()
        .map(|f| {
            format!(
                "{} {:?} {} {}",
                f.rel_path, f.content_hash, f.size, f.status
            )
        })
        .collect();
    rows.sort();
    rows
}

#[test]
fn failed_incremental_build_rolls_back_completely() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("r");
    for i in 0..20 {
        common::write(&root, &format!("m{i}.py"), &format!("x = {i}\n"));
    }
    let home = common::home(tmp.path());
    let layout = StorageLayout::new(&home);
    let mut cp = common::control(&home);
    let src = common::add_source(&mut cp, &root, &[], &[]);
    run_source(
        &layout,
        &mut cp,
        &src,
        &Registry::raw(),
        &opts(),
        &mut NoProgress,
    )
    .unwrap();
    let active = cp.state(&src.id).unwrap().active_build_id.unwrap();
    let before = snapshot(&layout, &src, &active);

    common::write(&root, "m1.py", "x = 'changed'\n");
    std::fs::remove_file(root.join("m2.py")).unwrap();
    common::write(&root, "new.py", "y = 1\n");
    let mut failing = Registry::raw();
    failing.finalizers.push(Arc::new(FailingFinalizer));
    let err = run_source(&layout, &mut cp, &src, &failing, &opts(), &mut NoProgress).unwrap_err();
    assert!(
        err.message().contains("finalizer failed"),
        "{}",
        err.message()
    );

    let state = cp.state(&src.id).unwrap();
    assert_eq!(state.active_build_id.as_deref(), Some(active.as_str()));
    assert_eq!(state.build_state, BuildState::Failed);
    assert_eq!(
        snapshot(&layout, &src, &active),
        before,
        "nothing of the failed pass is visible"
    );

    // The next good pass applies the same changes.
    let ok = run_source(
        &layout,
        &mut cp,
        &src,
        &Registry::raw(),
        &opts(),
        &mut NoProgress,
    )
    .unwrap();
    assert!(ok.published);
    assert_eq!(
        (ok.counts.changed, ok.counts.deleted, ok.counts.new),
        (1, 1, 1)
    );
    assert_eq!(cp.state(&src.id).unwrap().build_state, BuildState::Ready);
}

#[test]
fn failed_full_build_leaves_no_rows_and_previous_build_visible() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("r");
    for i in 0..10 {
        common::write(&root, &format!("m{i}.py"), &format!("x = {i}\n"));
    }
    let home = common::home(tmp.path());
    let layout = StorageLayout::new(&home);
    let mut cp = common::control(&home);
    let src = common::add_source(&mut cp, &root, &[], &[]);
    run_source(
        &layout,
        &mut cp,
        &src,
        &Registry::raw(),
        &opts(),
        &mut NoProgress,
    )
    .unwrap();
    let active = cp.state(&src.id).unwrap().active_build_id.unwrap();

    // A version change forces a full rebuild, which then fails.
    let mut failing = Registry::raw();
    failing.versions.parser_version = "raw-2".into();
    failing.finalizers.push(Arc::new(FailingFinalizer));
    run_source(&layout, &mut cp, &src, &failing, &opts(), &mut NoProgress).unwrap_err();
    assert_eq!(
        cp.state(&src.id).unwrap().active_build_id.as_deref(),
        Some(active.as_str())
    );
    let store =
        ProjectStore::open(&layout, &project_id_for_canonical(&src.path), &src.id, 8).unwrap();
    let builds: i64 = store
        .connection()
        .query_row("SELECT COUNT(*) FROM builds", [], |r| r.get(0))
        .unwrap();
    let rows: i64 = store
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM files WHERE build_id <> ?1",
            [&active],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        (builds, rows),
        (1, 0),
        "the failed build was rolled back entirely"
    );
    assert_eq!(store.files(&active).unwrap().len(), 10);
}

#[test]
fn readers_see_only_committed_state_during_a_build() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("r");
    for i in 0..5 {
        common::write(&root, &format!("m{i}.py"), &format!("x = {i}\n"));
    }
    let home = common::home(tmp.path());
    let layout = StorageLayout::new(&home);
    let mut cp = common::control(&home);
    let src = common::add_source(&mut cp, &root, &[], &[]);
    run_source(
        &layout,
        &mut cp,
        &src,
        &Registry::raw(),
        &opts(),
        &mut NoProgress,
    )
    .unwrap();
    let active = cp.state(&src.id).unwrap().active_build_id.unwrap();
    let old_hash = snapshot(&layout, &src, &active)
        .into_iter()
        .find(|r| r.starts_with("m0.py "))
        .unwrap();

    std::fs::remove_file(root.join("m4.py")).unwrap();
    common::write(&root, "m0.py", "x = 'edited'\n");
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut reg = Registry::raw();
    reg.code = Arc::new(Observer {
        db: layout.project_db(&project_id_for_canonical(&src.path)),
        build: active.clone(),
        seen: Arc::clone(&seen),
    });
    let r = run_source(&layout, &mut cp, &src, &reg, &opts(), &mut NoProgress).unwrap();
    assert!(r.published);
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    // Mid-build, the deletion and the edit are not yet visible.
    assert_eq!(seen[0].0, 5);
    assert!(old_hash.contains(seen[0].1.as_deref().unwrap()));
    let after = snapshot(&layout, &src, &active);
    assert_eq!(after.len(), 4);
    assert!(!after.contains(&old_hash));
}

/// Counts the files handed to the processor, by relative path.
#[derive(Default)]
struct Counting(std::sync::Mutex<Vec<String>>);

impl Processor for Counting {
    fn prepare(&self, input: &PrepareInput) -> Result<FileKnowledge, ProcessError> {
        self.0.lock().unwrap().push(input.rel_path.clone());
        Ok(FileKnowledge::default())
    }
}

fn counting_registry() -> (Registry, Arc<Counting>) {
    let c = Arc::new(Counting::default());
    let mut reg = Registry::raw();
    reg.code = c.clone();
    reg.document = c.clone();
    reg.unknown = c.clone();
    (reg, c)
}

#[test]
fn incremental_passes_process_only_changed_new_and_moved_files() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("r");
    for i in 0..200 {
        common::write(&root, &format!("pkg/m{i:03}.py"), &format!("x = {i}\n"));
    }
    let home = common::home(tmp.path());
    let layout = StorageLayout::new(&home);
    let mut cp = common::control(&home);
    let src = common::add_source(&mut cp, &root, &[], &[]);
    let (reg, seen) = counting_registry();
    run_source(&layout, &mut cp, &src, &reg, &opts(), &mut NoProgress).unwrap();
    assert_eq!(
        seen.0.lock().unwrap().len(),
        200,
        "cold build processes everything"
    );

    // A 1% change set: one edit, one rename, one delete, one new file.
    seen.0.lock().unwrap().clear();
    common::write(&root, "pkg/m010.py", "x = 'edited'\n");
    std::fs::rename(root.join("pkg/m020.py"), root.join("pkg/renamed.py")).unwrap();
    std::fs::remove_file(root.join("pkg/m030.py")).unwrap();
    common::write(&root, "pkg/new.py", "y = 1\n");
    let r = run_source(&layout, &mut cp, &src, &reg, &opts(), &mut NoProgress).unwrap();
    let mut processed = seen.0.lock().unwrap().clone();
    processed.sort();
    assert_eq!(processed, ["pkg/m010.py", "pkg/new.py", "pkg/renamed.py"]);
    assert_eq!(
        (
            r.counts.changed,
            r.counts.new,
            r.counts.moved,
            r.counts.deleted
        ),
        (1, 1, 1, 1)
    );
    // Hashing is metadata-driven: only paths whose size/mtime changed or
    // that are new are hashed (the new and renamed paths for move
    // detection), never the 197 untouched files.
    assert_eq!(r.timings.hash_calls, 3);
    let files = visible_files(&layout, &cp, &src);
    assert_eq!(files.len(), 200);
    assert!(files
        .iter()
        .all(|(p, _)| p != "pkg/m030.py" && p != "pkg/m020.py"));

    // A touched-but-identical file is re-statted, not reprocessed.
    seen.0.lock().unwrap().clear();
    common::write(&root, "pkg/m050.py", "x = 50\n");
    let r = run_source(&layout, &mut cp, &src, &reg, &opts(), &mut NoProgress).unwrap();
    assert!(
        seen.0.lock().unwrap().is_empty(),
        "identical content is never reprocessed"
    );
    assert!(!r.published);
}

#[test]
fn a_build_left_unpublished_by_a_crash_is_discarded_on_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("r");
    for i in 0..10 {
        common::write(&root, &format!("m{i}.py"), &format!("x = {i}\n"));
    }
    let home = common::home(tmp.path());
    let layout = StorageLayout::new(&home);
    let mut cp = common::control(&home);
    let src = common::add_source(&mut cp, &root, &[], &[]);
    let reg = Registry::raw();
    let first = run_source(&layout, &mut cp, &src, &reg, &opts(), &mut NoProgress).unwrap();
    let published = first.build_id.unwrap();

    // Simulate a process that committed a full build's rows and died before
    // publishing it: the control plane still names it as pending.
    let orphan = "orphan-build";
    {
        let mut store =
            ProjectStore::open(&layout, &project_id_for_canonical(&src.path), &src.id, 8).unwrap();
        store.create_build(orphan, true, &reg.versions).unwrap();
    }
    cp.require_full_rebuild(&src.id, "test").unwrap();
    cp.begin_build(&src.id, orphan).unwrap();
    assert_eq!(
        visible_files(&layout, &cp, &src).len(),
        10,
        "orphan is invisible"
    );

    let r = run_source(&layout, &mut cp, &src, &reg, &opts(), &mut NoProgress).unwrap();
    assert_eq!(r.recovered_build.as_deref(), Some(orphan));
    assert!(r.published);
    let state = cp.state(&src.id).unwrap();
    assert_eq!(state.pending_build_id, None);
    assert_ne!(state.active_build_id.as_deref(), Some(published.as_str()));
    let store =
        ProjectStore::open(&layout, &project_id_for_canonical(&src.path), &src.id, 8).unwrap();
    let builds: Vec<(String, String)> = {
        let mut stmt = store
            .connection()
            .prepare("SELECT id, status FROM builds ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(builds.len(), 1, "exactly one build remains: {builds:?}");
    assert_eq!(builds[0].1, "published");
    assert_eq!(visible_files(&layout, &cp, &src).len(), 10);
}

#[test]
fn pipeline_concurrency_stays_within_its_bounds() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("r");
    for i in 0..300 {
        common::write(&root, &format!("f{i}.py"), &format!("x = {i}\n"));
    }
    let home = common::home(tmp.path());
    let layout = StorageLayout::new(&home);
    let mut cp = common::control(&home);
    let src = common::add_source(&mut cp, &root, &[], &[]);
    let o = opts();
    let r = run_source(
        &layout,
        &mut cp,
        &src,
        &Registry::raw(),
        &o,
        &mut NoProgress,
    )
    .unwrap();
    let p = r.pipeline;
    assert_eq!((p.workers, p.items), (o.workers, 300));
    assert!(p.active_workers_high_water >= 1 && p.active_workers_high_water <= o.workers);
    assert!(
        p.in_flight_high_water <= 2 * p.queue_capacity + p.workers + 2,
        "in-flight {} exceeds the channel bounds ({} x2 + {} workers + 2)",
        p.in_flight_high_water,
        p.queue_capacity,
        p.workers
    );
}
