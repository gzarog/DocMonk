//! Bounded execution and cooperative cancellation (audit PH05: T05.3, F09).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use docmonk_core::deadline;
use docmonk_mcp::exec::{Executor, Limits, Outcome};

/// Work that keeps running until its request is cancelled, like a
/// retrieval loop checking the deadline between units of work.
fn cooperative(live: Arc<AtomicUsize>) -> impl FnOnce() -> bool + Send + 'static {
    move || {
        live.fetch_add(1, Ordering::SeqCst);
        let started = Instant::now();
        while deadline::check().is_ok() {
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "never cancelled"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        live.fetch_sub(1, Ordering::SeqCst);
        true
    }
}

#[test]
fn a_hundred_concurrent_timed_out_calls_never_exceed_the_caps() {
    let exec = Executor::new(Limits {
        max_running: 4,
        max_queued: 8,
    });
    let live = Arc::new(AtomicUsize::new(0));
    let outcomes: Vec<&'static str> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..100)
            .map(|_| {
                let exec = exec.clone();
                let live = live.clone();
                s.spawn(
                    move || match exec.run(Duration::from_millis(40), cooperative(live)) {
                        Outcome::TimedOut => "timeout",
                        Outcome::Overloaded => "overloaded",
                        Outcome::Done(_) => "done",
                        Outcome::Panicked => "panic",
                    },
                )
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let m = exec.metrics();
    assert!(m.peak_running <= 4, "{m:?}");
    assert!(m.peak_queued <= 8, "{m:?}");
    // "done" is a worker that saw its own deadline and stopped just before
    // the caller's wait ran out (run_tool reports that as a timeout too).
    assert!(outcomes.iter().all(|o| *o != "panic"), "{outcomes:?}");
    assert!(outcomes.contains(&"overloaded"), "backpressure engaged");
    assert_eq!(
        m.rejected as usize,
        outcomes.iter().filter(|o| **o == "overloaded").count()
    );
    // Cancelled workers stop and release their slots: nothing accumulates.
    assert!(exec.drain(Duration::from_secs(10)), "{:?}", exec.metrics());
    assert_eq!(live.load(Ordering::SeqCst), 0);
    assert_eq!(exec.metrics().running, 0);
}

#[test]
fn repeated_retries_reuse_slots_instead_of_piling_up() {
    let exec = Executor::new(Limits {
        max_running: 2,
        max_queued: 0,
    });
    let live = Arc::new(AtomicUsize::new(0));
    for _ in 0..20 {
        let _ = exec.run(Duration::from_millis(10), cooperative(live.clone()));
        assert!(exec.metrics().running <= 2);
    }
    assert!(exec.drain(Duration::from_secs(10)));
    assert_eq!(live.load(Ordering::SeqCst), 0);
}

#[test]
fn results_and_panics_are_reported_without_leaking_payloads() {
    let exec = Executor::new(Limits {
        max_running: 1,
        max_queued: 0,
    });
    assert!(matches!(
        exec.run(Duration::from_secs(5), || 7),
        Outcome::Done(7)
    ));
    let p = exec.run(Duration::from_secs(5), || -> u8 {
        panic!("secret /home/x token=abc")
    });
    assert!(matches!(p, Outcome::Panicked));
    assert_eq!(exec.metrics().running, 0);
}

#[test]
fn the_deadline_reaches_sqlite_and_late_results_are_discarded() {
    let exec = Executor::new(Limits {
        max_running: 1,
        max_queued: 0,
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    let out = exec.run(Duration::from_millis(50), move || {
        let conn = docmonk_storage::db::open(&path, 1).unwrap();
        conn.query_row(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n) SELECT count(*) FROM n",
            [],
            |r| r.get::<_, i64>(0),
        )
        .is_err()
    });
    // Either the worker noticed in time (interrupted, Done(true)) or the
    // caller timed out first; in both cases the worker stops promptly.
    assert!(matches!(out, Outcome::Done(true) | Outcome::TimedOut));
    assert!(exec.drain(Duration::from_secs(10)));
}
