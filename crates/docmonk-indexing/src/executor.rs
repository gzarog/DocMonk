//! The bounded parallel source executor (ADR 0033, P0-I01).
//!
//! [`run_bounded`] runs one job per item on at most `parallel` scoped
//! threads. Items are claimed in order, so the start order is the item
//! order. Each job's events reach the caller's `on_event` on the calling
//! thread, through a bounded channel (a slow consumer blocks producers; no
//! unbounded queue). A job that panics fails only its own item; every other
//! item still runs. Nothing outlives the call: all threads are joined
//! before it returns.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};

/// Concurrency observed by one [`run_bounded`] call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExecutorStats {
    /// Threads used: `min(parallel, items)`, at least one.
    pub workers: usize,
    /// Most jobs running at the same time.
    pub running_high_water: usize,
    /// Jobs that panicked.
    pub panicked: usize,
}

/// Where a job sends its events.
pub struct Emitter<'a, E> {
    index: usize,
    tx: &'a SyncSender<(usize, E)>,
}

impl<E> Emitter<'_, E> {
    /// Sends one event for this item. `false` once the consumer is gone.
    pub fn emit(&self, event: E) -> bool {
        self.tx.send((self.index, event)).is_ok()
    }
}

/// Runs `job` for every item, at most `parallel` at once. `on_panic`
/// builds the event reported for an item whose job panicked.
pub fn run_bounded<T, E, J, P, C>(
    items: &[T],
    parallel: usize,
    job: J,
    on_panic: P,
    mut on_event: C,
) -> ExecutorStats
where
    T: Sync,
    E: Send,
    J: Fn(&T, &Emitter<'_, E>) + Sync,
    P: Fn(&T) -> E + Sync,
    C: FnMut(usize, E),
{
    let workers = parallel.max(1).min(items.len().max(1));
    let next = AtomicUsize::new(0);
    let running = AtomicUsize::new(0);
    let high = AtomicUsize::new(0);
    let panicked = AtomicUsize::new(0);
    let (tx, rx) = sync_channel::<(usize, E)>(workers * 2);
    std::thread::scope(|s| {
        for _ in 0..workers {
            let tx = tx.clone();
            let (next, running, high, panicked, job, on_panic) =
                (&next, &running, &high, &panicked, &job, &on_panic);
            s.spawn(move || loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                let Some(item) = items.get(i) else { break };
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                high.fetch_max(now, Ordering::SeqCst);
                let emitter = Emitter { index: i, tx: &tx };
                let ok =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(item, &emitter)))
                        .is_ok();
                running.fetch_sub(1, Ordering::SeqCst);
                if !ok {
                    panicked.fetch_add(1, Ordering::SeqCst);
                    if tx.send((i, on_panic(item))).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        for (i, e) in rx {
            on_event(i, e);
        }
    });
    ExecutorStats {
        workers,
        running_high_water: high.into_inner(),
        panicked: panicked.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Condvar, Mutex};
    use std::time::Duration;

    /// A barrier that gives up after a timeout (a plain `Barrier` would
    /// hang a failing test forever).
    struct TimedBarrier {
        n: usize,
        state: Mutex<usize>,
        cv: Condvar,
    }

    impl TimedBarrier {
        fn new(n: usize) -> Self {
            Self {
                n,
                state: Mutex::new(0),
                cv: Condvar::new(),
            }
        }
        /// True when all `n` parties arrived within `timeout`.
        fn wait(&self, timeout: Duration) -> bool {
            let mut g = self.state.lock().unwrap();
            *g += 1;
            self.cv.notify_all();
            let (g, r) = self
                .cv
                .wait_timeout_while(g, timeout, |c| *c < self.n)
                .unwrap();
            drop(g);
            !r.timed_out()
        }
    }

    #[test]
    fn independent_items_run_concurrently() {
        // Both jobs must be inside the barrier at the same time.
        let barrier = TimedBarrier::new(2);
        let mut met = Vec::new();
        let stats = run_bounded(
            &["a", "b"],
            2,
            |_, e| {
                e.emit(barrier.wait(Duration::from_secs(10)));
            },
            |_| false,
            |_, ok| met.push(ok),
        );
        assert_eq!(met, [true, true]);
        assert_eq!(stats.running_high_water, 2);
    }

    #[test]
    fn a_cap_of_one_is_strictly_serial() {
        let barrier = TimedBarrier::new(2);
        let mut met = Vec::new();
        let stats = run_bounded(
            &["a", "b"],
            1,
            |_, e| {
                e.emit(barrier.wait(Duration::from_millis(100)));
            },
            |_| false,
            |_, ok| met.push(ok),
        );
        assert_eq!(met, [false, true], "the second job never overlapped");
        assert_eq!(stats.running_high_water, 1);
    }

    #[test]
    fn at_most_parallel_jobs_run_and_every_item_runs_once_in_order() {
        let items: Vec<usize> = (0..150).collect();
        let live = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let started = Mutex::new(Vec::new());
        let mut done = vec![0u32; 150];
        let stats = run_bounded(
            &items,
            4,
            |i, e| {
                started.lock().unwrap().push(*i);
                let n = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(n, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(1));
                live.fetch_sub(1, Ordering::SeqCst);
                e.emit(*i);
            },
            |_| usize::MAX,
            |idx, v| {
                assert_eq!(idx, v);
                done[v] += 1;
            },
        );
        assert!(peak.load(Ordering::SeqCst) <= 4);
        assert!(stats.running_high_water <= 4);
        assert!(done.iter().all(|&n| n == 1), "every source exactly once");
        // Claimed in order: a source is never started before an earlier one.
        let s = started.into_inner().unwrap();
        let mut claims = s.clone();
        claims.sort();
        assert_eq!(claims, items);
        assert_eq!(stats.workers, 4);
    }

    #[test]
    fn failing_and_panicking_items_do_not_block_others() {
        let items = ["ok1", "fail", "panic", "ok2", "ok3"];
        let mut events = Vec::new();
        let stats = run_bounded(
            &items,
            2,
            |item, e| match *item {
                "fail" => {
                    e.emit(Err(item.to_string()));
                }
                "panic" => panic!("boom"),
                _ => {
                    e.emit(Ok(item.to_string()));
                }
            },
            |item| Err(format!("{item} panicked")),
            |_, ev| events.push(ev),
        );
        assert_eq!(stats.panicked, 1);
        let oks = events.iter().filter(|e| e.is_ok()).count();
        assert_eq!(oks, 3, "{events:?}");
        assert!(events.contains(&Err("panic panicked".into())));
        assert!(events.contains(&Err("fail".into())));
    }
}
