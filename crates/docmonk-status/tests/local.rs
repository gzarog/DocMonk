//! Local collector against real SQLite homes: counts equal direct SQL,
//! errors are globally ordered, runs are per source, and a store that
//! cannot be read is unknown, never zero.

mod common;

use std::time::Instant;

use chrono::Utc;
use docmonk_core::paths::project_id_for_canonical;
use docmonk_indexing::progress::{iso, write_progress, IndexProgress, SourceProgress};
use docmonk_status::local::{collect, LocalInputs};
use docmonk_status::model::*;
use docmonk_status::CollectOptions;
use docmonk_storage::StorageLayout;

fn report(home: &docmonk_core::paths::Home) -> StatusReport {
    let cp = common::control(home);
    collect(
        &LocalInputs {
            home,
            control: &cp,
            cache_size_mb: 8,
        },
        &CollectOptions::default(),
    )
    .unwrap()
}

fn sql(home: &docmonk_core::paths::Home, path: &str, q: &str) -> i64 {
    let layout = StorageLayout::new(home);
    let db = layout.project_db(&project_id_for_canonical(path));
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row(q, [], |r| r.get(0)).unwrap()
}

#[test]
fn empty_home_is_a_valid_healthy_report() {
    let tmp = tempfile::tempdir().unwrap();
    let home = common::home(tmp.path());
    let r = report(&home);
    assert_eq!(r.mode, Mode::Local);
    assert_eq!(r.backend.kind, BackendKind::Sqlite);
    assert!(r.sources.is_empty());
    assert_eq!(r.health.state, HealthState::Healthy);
    assert_eq!(r.summary.files.discovered, Some(0));
    assert_eq!(r.summary.files.pending_in_current_runs, None);
    assert_eq!(r.indexer.active_run_count, 0);
    assert_eq!(r.diagnostics.request_count, Some(2));
    assert!(!r.diagnostics.partial);
}

#[test]
fn one_source_counts_match_sql_and_states_are_precise() {
    let tmp = tempfile::tempdir().unwrap();
    let home = common::home(tmp.path());
    let mut cp = common::control(&home);
    let root = tmp.path().join("r");
    for f in ["ok.py", "ok2.py", "retry.py", "bad.py"] {
        common::write(&root, f, f);
    }
    let src = common::add_source(&mut cp, &root);
    common::index(&home, &mut cp);
    let r = report(&home);
    let s = r.source(&src.id).unwrap();
    let p = s.published.as_ref().unwrap();
    let build = s.build.active_id.clone().unwrap();
    let q = |w: &str| {
        sql(
            &home,
            &src.path,
            &format!("SELECT COUNT(*) FROM files WHERE build_id = '{build}'{w}"),
        )
    };
    assert_eq!(p.files as i64, q(""));
    assert_eq!(p.indexed as i64, q(" AND status = 'indexed'"));
    assert_eq!((p.indexed, p.failed, p.retrying), (2, 1, 1));
    assert!(p.next_retry_at.is_some());
    assert_eq!(s.index_state, SourceIndexState::Retrying);
    assert_eq!(s.access, Access::Online);
    assert_eq!(s.build.state, BuildState::Ready);
    assert!(s.build.published_at.is_some());
    assert!(s.last_error_at.is_some());
    let codes: Vec<ProblemCode> = r.problems.iter().map(|p| p.code).collect();
    assert!(codes.contains(&ProblemCode::FilesFailed), "{codes:?}");
    assert!(codes.contains(&ProblemCode::FilesRetrying), "{codes:?}");
    assert_eq!(r.health.state, HealthState::Degraded);
    assert_eq!(r.recent_errors.len(), 2);
    assert!(r.recent_errors[0].occurred_at >= r.recent_errors[1].occurred_at);
    assert_eq!(r.summary.files.published_indexed, Some(2));
    // Finished run of index_all is not live.
    assert_eq!(r.indexer.active_run_count, 0);
}

#[test]
fn rebuilt_generation_replaces_counts_and_offline_disabled_are_explicit() {
    let tmp = tempfile::tempdir().unwrap();
    let home = common::home(tmp.path());
    let mut cp = common::control(&home);
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    let c = tmp.path().join("c");
    common::write(&a, "x.py", "1");
    common::write(&b, "y.py", "1");
    common::write(&c, "z.py", "1");
    let sa = common::add_source(&mut cp, &a);
    let sb = common::add_source(&mut cp, &b);
    let sc = common::add_source(&mut cp, &c);
    common::index(&home, &mut cp);
    let first = report(&home)
        .source(&sa.id)
        .unwrap()
        .build
        .active_id
        .clone();
    // Second generation of a: two more files, full rebuild.
    common::write(&a, "x2.py", "2");
    common::write(&a, "x3.py", "3");
    cp.require_full_rebuild(&sa.id, "test").unwrap();
    std::fs::remove_dir_all(&b).unwrap();
    cp.set_enabled(&sc.id, false).unwrap();
    common::index(&home, &mut cp);
    let r = report(&home);
    let s = r.source(&sa.id).unwrap();
    assert_ne!(s.build.active_id, first, "a new generation was published");
    assert_eq!(s.published.as_ref().unwrap().files, 3);
    assert_eq!(
        r.source(&sb.id).unwrap().index_state,
        SourceIndexState::Offline
    );
    assert_eq!(r.source(&sb.id).unwrap().access, Access::Offline);
    assert_eq!(
        r.source(&sc.id).unwrap().index_state,
        SourceIndexState::Disabled
    );
    assert_eq!(r.summary.sources.offline, 1);
    assert_eq!(r.summary.sources.enabled, 2);
    assert!(r
        .problems
        .iter()
        .any(|p| p.code == ProblemCode::SourceOffline && p.source_id.as_deref() == Some(&sb.id)));
}

#[test]
fn errors_are_globally_ordered_with_ties_across_sources() {
    let tmp = tempfile::tempdir().unwrap();
    let home = common::home(tmp.path());
    let mut cp = common::control(&home);
    let mut srcs = Vec::new();
    for n in ["s1", "s2", "s3"] {
        let root = tmp.path().join(n);
        common::write(&root, "ok.py", "1");
        srcs.push(common::add_source(&mut cp, &root));
    }
    common::index(&home, &mut cp);
    let layout = StorageLayout::new(&home);
    // Same timestamp in every source plus one newer and one older error.
    let rows = [
        (0, "e1", "2026-10-08T10:00:00.000000+00:00", "p/b"),
        (1, "e2", "2026-10-08T10:00:00.000000+00:00", "p/a"),
        (2, "e3", "2026-10-08T10:00:00.000000+00:00", "p/c"),
        (2, "e4", "2026-10-08T12:00:00.000000+00:00", "p/new"),
        (0, "e5", "2026-10-08T08:00:00.000000+00:00", "p/old"),
    ];
    for (i, id, at, path) in rows {
        let s = &srcs[i];
        let conn =
            rusqlite::Connection::open(layout.project_db(&project_id_for_canonical(&s.path)))
                .unwrap();
        conn.execute(
            "INSERT INTO index_errors (id, source_id, rel_path, error_code, error_message, occurred_at)
             VALUES (?1, ?2, ?3, 'parse', 'bad', ?4)",
            rusqlite::params![id, s.id, path, at],
        )
        .unwrap();
    }
    let r = report(&home);
    let got: Vec<(&str, &str)> = r
        .recent_errors
        .iter()
        .map(|e| {
            (
                e.occurred_at.as_deref().unwrap(),
                e.path.as_deref().unwrap(),
            )
        })
        .collect();
    assert_eq!(got[0], ("2026-10-08T12:00:00.000000Z", "p/new"));
    let ties: Vec<&str> = r.recent_errors[1..4]
        .iter()
        .map(|e| e.source_id.as_str())
        .collect();
    let mut sorted = ties.clone();
    sorted.sort_unstable();
    assert_eq!(ties, sorted, "ties break by source id");
    assert_eq!(got[4], ("2026-10-08T08:00:00.000000Z", "p/old"));
}

#[test]
fn hundred_fifty_sources_are_read_with_bounded_statements() {
    let tmp = tempfile::tempdir().unwrap();
    let home = common::home(tmp.path());
    let mut cp = common::control(&home);
    let mut want_files = 0;
    for n in 0..150 {
        let root = tmp.path().join(format!("src{n:03}"));
        common::write(&root, "a.py", "a");
        if n % 10 == 0 {
            common::write(&root, "bad.py", "b");
            want_files += 1;
        }
        want_files += 1;
        common::add_source(&mut cp, &root);
    }
    common::index(&home, &mut cp);
    let started = Instant::now();
    let r = report(&home);
    let elapsed = started.elapsed();
    assert_eq!(r.sources.len(), 150);
    assert_eq!(r.summary.files.discovered, Some(want_files));
    assert_eq!(r.summary.files.failed, Some(15));
    // Catalog + states + three statements per published source (build
    // summary, errors, dependency manifest).
    assert!(r.diagnostics.request_count.unwrap() <= 2 + 3 * 150);
    assert_eq!(r.recent_errors.len(), DEFAULT_ERROR_LIMIT.min(15));
    eprintln!(
        "local 150-source snapshot: {:?}, {} statements",
        elapsed,
        r.diagnostics.request_count.unwrap()
    );
}

#[test]
fn status_reads_while_a_writer_holds_the_store() {
    let tmp = tempfile::tempdir().unwrap();
    let home = common::home(tmp.path());
    let mut cp = common::control(&home);
    let root = tmp.path().join("r");
    common::write(&root, "ok.py", "1");
    let src = common::add_source(&mut cp, &root);
    common::index(&home, &mut cp);
    let layout = StorageLayout::new(&home);
    let mut w = rusqlite::Connection::open(layout.project_db(&project_id_for_canonical(&src.path)))
        .unwrap();
    let tx = w
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    tx.execute(
        "INSERT INTO index_errors (id, source_id, rel_path, error_code, error_message, occurred_at)
         VALUES ('w', ?1, 'x', 'c', 'm', '2026-10-08T10:00:00+00:00')",
        [&src.id],
    )
    .unwrap();
    let started = Instant::now();
    let r = report(&home);
    assert!(
        started.elapsed().as_secs() < 3,
        "WAL readers never wait for the writer"
    );
    assert!(r.recent_errors.is_empty(), "uncommitted rows are invisible");
    tx.commit().unwrap();
    assert_eq!(report(&home).recent_errors.len(), 1);
}

#[test]
fn unreadable_project_store_is_unknown_not_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let home = common::home(tmp.path());
    let mut cp = common::control(&home);
    let root = tmp.path().join("r");
    common::write(&root, "ok.py", "1");
    let src = common::add_source(&mut cp, &root);
    common::index(&home, &mut cp);
    let db = StorageLayout::new(&home).project_db(&project_id_for_canonical(&src.path));
    for ext in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", db.display()));
    }
    std::fs::write(&db, b"this is not a database at all, not even close......").unwrap();
    let r = report(&home);
    let s = r.source(&src.id).unwrap();
    assert!(s.published.is_none());
    assert_eq!(s.index_state, SourceIndexState::Unknown);
    assert!(r.diagnostics.partial);
    assert_eq!(r.summary.files.discovered, None);
    assert_eq!(r.health.state, HealthState::Unknown);
}

#[test]
fn four_active_passes_crash_and_stall_are_judged_per_source() {
    let tmp = tempfile::tempdir().unwrap();
    let home = common::home(tmp.path());
    let mut cp = common::control(&home);
    let mut ids = Vec::new();
    for n in 0..4 {
        let root = tmp.path().join(format!("s{n}"));
        common::write(&root, "a.py", "1");
        ids.push(common::add_source(&mut cp, &root).id);
    }
    let now = Utc::now();
    let fresh = iso(now);
    let stages = ["scanning", "processing", "finalizing", "publishing"];
    let mut p = IndexProgress {
        running: true,
        run_id: Some("run-x".into()),
        pid: Some(i64::from(std::process::id())),
        host: Some("me".into()),
        operation: Some("index".into()),
        max_parallel_sources: Some(4),
        updated_at: Some(fresh.clone()),
        sources: ids
            .iter()
            .zip(stages)
            .map(|(id, st)| SourceProgress {
                stage: Some(st.into()),
                planned: (st != "scanning").then_some(10),
                processed: 4,
                ..SourceProgress::new(id, &fresh)
            })
            .collect(),
        ..IndexProgress::default()
    };
    write_progress(&home.index_progress(), &p).unwrap();
    let r = report(&home);
    let states: Vec<SourceIndexState> = ids
        .iter()
        .map(|id| r.source(id).unwrap().index_state)
        .collect();
    assert_eq!(
        states,
        [
            SourceIndexState::Scanning,
            SourceIndexState::Indexing,
            SourceIndexState::Finalizing,
            SourceIndexState::Publishing
        ]
    );
    assert_eq!(
        (r.indexer.active_run_count, r.indexer.active_source_count),
        (1, 4)
    );
    let scanning = r.source(&ids[0]).unwrap().live.as_ref().unwrap();
    assert_eq!(
        scanning.percentage, None,
        "no denominator before the scan finished"
    );
    assert_eq!(
        r.source(&ids[1]).unwrap().live.as_ref().unwrap().percentage,
        Some(40.0)
    );
    assert_eq!(r.summary.files.pending_in_current_runs, None);

    // One pass stops heartbeating: stalled, the others stay live.
    p.sources[1].heartbeat_at = Some(iso(now - chrono::Duration::seconds(900)));
    write_progress(&home.index_progress(), &p).unwrap();
    let r = report(&home);
    assert_eq!(
        r.source(&ids[1]).unwrap().index_state,
        SourceIndexState::Stalled
    );
    assert_eq!(
        r.source(&ids[0]).unwrap().index_state,
        SourceIndexState::Scanning
    );
    assert!(r.problems.iter().any(|x| x.code == ProblemCode::RunStalled));
    assert_eq!(r.health.state, HealthState::Failed);

    // The process is gone: nothing is live, the run crashed.
    p.pid = Some(dead_pid());
    write_progress(&home.index_progress(), &p).unwrap();
    let r = report(&home);
    assert_eq!(r.indexer.active_source_count, 0);
    assert!(ids
        .iter()
        .all(|id| r.source(id).unwrap().index_state == SourceIndexState::Stalled));
    assert_eq!(r.indexer.last_run.as_ref().unwrap().outcome, "crashed");
    assert!(r.problems.iter().any(|x| x.code == ProblemCode::RunCrashed));
}

fn dead_pid() -> i64 {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--list")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let pid = i64::from(child.id());
    child.wait().unwrap();
    pid
}

/// Peak resident memory of this process (Linux), in KiB.
fn peak_rss_kib() -> Option<u64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find(|l| l.starts_with("VmHWM:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// Benchmark (ignored by default): 150 local sources, 30 snapshots.
#[test]
#[ignore = "benchmark: cargo test -p docmonk-status --release --test local -- --ignored"]
fn bench_local_150_sources() {
    let tmp = tempfile::tempdir().unwrap();
    let home = common::home(tmp.path());
    let mut cp = common::control(&home);
    for n in 0..150 {
        let root = tmp.path().join(format!("src{n:03}"));
        for f in 0..10 {
            common::write(&root, &format!("m{f}.py"), "def f():\n    return 1\n");
        }
        if n % 10 == 0 {
            common::write(&root, "bad.py", "b");
        }
        common::add_source(&mut cp, &root);
    }
    common::index(&home, &mut cp);
    let mut ms = Vec::new();
    let mut statements = 0;
    for _ in 0..30 {
        let t = Instant::now();
        let r = report(&home);
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
        statements = r.diagnostics.request_count.unwrap();
    }
    ms.sort_by(f64::total_cmp);
    let p = |q: f64| (ms[((ms.len() as f64 - 1.0) * q).round() as usize] * 10.0).round() / 10.0;
    println!(
        "BENCH {}",
        serde_json::json!({
            "mode": "local", "sources": 150, "files": 1515, "samples": 30,
            "sql_statements": statements, "p50_ms": p(0.5), "p95_ms": p(0.95),
            "peak_rss_kib": peak_rss_kib(),
        })
    );
}

#[test]
fn a_graph_build_reports_its_phase_and_derivation_progress() {
    let tmp = tempfile::tempdir().unwrap();
    let home = common::home(tmp.path());
    let mut cp = common::control(&home);
    let root = tmp.path().join("s");
    common::write(&root, "a.py", "1");
    let id = common::add_source(&mut cp, &root).id;
    let fresh = iso(Utc::now());
    let p = IndexProgress {
        running: true,
        run_id: Some("run-g".into()),
        pid: Some(i64::from(std::process::id())),
        host: Some("me".into()),
        operation: Some("relationships".into()),
        updated_at: Some(fresh.clone()),
        sources: vec![SourceProgress {
            stage: Some("relationships".into()),
            relationships: Some("building".into()),
            relationships_planned: Some(40),
            relationships_processed: 40,
            relationships_phase: Some("resolving".into()),
            ..SourceProgress::new(&id, &fresh)
        }],
        ..IndexProgress::default()
    };
    write_progress(&home.index_progress(), &p).unwrap();
    let r = report(&home);
    let live = r.source(&id).unwrap().live.as_ref().unwrap();
    let g = live.relationships.as_ref().expect("graph progress");
    assert_eq!(
        (g.phase.as_deref(), g.processed, g.planned),
        (Some("resolving"), Some(40), Some(40)),
        "every file derived, the build still resolving"
    );
    let json = serde_json::to_value(live).unwrap();
    assert_eq!(json["relationships"]["phase"], "resolving");
}
