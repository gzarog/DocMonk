//! Per-key debounce: each
//! `notify(key)` restarts that key's quiet period. The key fires once a
//! full quiet period passes with no further notify. Keys never delay each
//! other. One timer thread serves every key.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

type OnFire = Box<dyn Fn(String) + Send + Sync>;

#[derive(Default)]
struct State {
    due: HashMap<String, Instant>,
    stopped: bool,
}

struct Shared {
    state: Mutex<State>,
    cv: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

pub struct Debouncer {
    quiet: Duration,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Debouncer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Debouncer")
            .field("quiet", &self.quiet)
            .finish_non_exhaustive()
    }
}

impl Debouncer {
    /// `quiet_ms` below zero counts as zero.
    pub fn new(quiet_ms: i64, on_fire: impl Fn(String) + Send + Sync + 'static) -> Self {
        let quiet = Duration::from_millis(quiet_ms.max(0) as u64);
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            cv: Condvar::new(),
        });
        let s = Arc::clone(&shared);
        let on_fire: OnFire = Box::new(on_fire);
        let thread = std::thread::Builder::new()
            .name("ragmonk-debounce".into())
            .spawn(move || run(&s, &on_fire))
            .ok();
        Self {
            quiet,
            shared,
            thread,
        }
    }

    /// (Re)starts `key`'s quiet period.
    pub fn notify(&self, key: impl Into<String>) {
        let mut g = self.shared.lock();
        if g.stopped {
            return;
        }
        g.due.insert(key.into(), Instant::now() + self.quiet);
        drop(g);
        self.shared.cv.notify_all();
    }

    /// Drops every pending key without firing it (shutdown). Reconciliation
    /// on the next start catches anything dropped here.
    pub fn flush(&self) {
        self.shared.lock().due.clear();
        self.shared.cv.notify_all();
    }

    pub fn pending_count(&self) -> usize {
        self.shared.lock().due.len()
    }
}

impl Drop for Debouncer {
    fn drop(&mut self) {
        {
            let mut g = self.shared.lock();
            g.stopped = true;
            g.due.clear();
        }
        self.shared.cv.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(shared: &Shared, on_fire: &OnFire) {
    let mut g = shared.lock();
    loop {
        if g.stopped {
            return;
        }
        let now = Instant::now();
        let mut ready: Vec<String> = g
            .due
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(k, _)| k.clone())
            .collect();
        if !ready.is_empty() {
            ready.sort();
            for k in &ready {
                g.due.remove(k);
            }
            // Fire outside the lock: a callback may notify again.
            drop(g);
            for k in ready {
                on_fire(k);
            }
            g = shared.lock();
            continue;
        }
        g = match g.due.values().min().copied() {
            Some(next) => {
                shared
                    .cv
                    .wait_timeout(g, next.saturating_duration_since(now))
                    .unwrap_or_else(|p| p.into_inner())
                    .0
            }
            None => shared.cv.wait(g).unwrap_or_else(|p| p.into_inner()),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collector() -> (Arc<Mutex<Vec<String>>>, impl Fn(String) + Send + Sync) {
        let fired = Arc::new(Mutex::new(Vec::new()));
        let f = Arc::clone(&fired);
        (fired, move |k| f.lock().unwrap().push(k))
    }

    #[test]
    fn burst_collapses_and_keys_are_independent() {
        let (fired, cb) = collector();
        let d = Debouncer::new(80, cb);
        for _ in 0..10 {
            d.notify("a");
            std::thread::sleep(Duration::from_millis(10));
        }
        d.notify("b");
        assert_eq!(d.pending_count(), 2);
        std::thread::sleep(Duration::from_millis(300));
        let mut got = fired.lock().unwrap().clone();
        got.sort();
        assert_eq!(got, ["a", "b"]);
        assert_eq!(d.pending_count(), 0);
    }

    #[test]
    fn flush_drops_pending() {
        let (fired, cb) = collector();
        let d = Debouncer::new(100, cb);
        d.notify("a");
        d.flush();
        std::thread::sleep(Duration::from_millis(250));
        assert!(fired.lock().unwrap().is_empty());
    }

    #[test]
    fn zero_quiet_fires_promptly() {
        let (fired, cb) = collector();
        let d = Debouncer::new(-5, cb);
        d.notify("a");
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(*fired.lock().unwrap(), ["a"]);
    }
}
