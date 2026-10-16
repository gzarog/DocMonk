//! Bounded execution of tool calls (audit F09).
//!
//! At most `max_running` calls run at once and at most `max_queued` wait
//! for a slot; past that a call fails fast with [`Outcome::Overloaded`]
//! instead of starting another thread. Each call runs on its own worker
//! with a [`Token`] whose deadline is the call's timeout: retrieval,
//! SQLite, HTTP and embedding check it and stop, so a timed-out call frees
//! its slot as soon as its current unit of work ends. Its late result is
//! discarded (quarantined), never returned. A worker keeps its slot until
//! it has really stopped, so `running` always counts live work.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use docmonk_core::deadline::Token;
use serde_json::{json, Value};

/// Limits for one executor (from the `mcp` config section).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_running: usize,
    pub max_queued: usize,
}

/// Counters exposed for monitoring.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Metrics {
    /// Workers alive now (including cancelled ones still stopping).
    pub running: usize,
    /// Calls waiting for a slot now.
    pub queued: usize,
    pub completed: u64,
    /// Calls refused because the queue was full.
    pub rejected: u64,
    /// Calls whose deadline passed (waiting or running).
    pub timed_out: u64,
    /// Results of timed-out workers that arrived late and were discarded.
    pub discarded_late: u64,
    /// Highest `running` ever observed (never above `max_running`).
    pub peak_running: usize,
    /// Highest `queued` ever observed (never above `max_queued`).
    pub peak_queued: usize,
}

impl Metrics {
    pub fn to_json(self) -> Value {
        json!({
            "running": self.running,
            "queued": self.queued,
            "completed": self.completed,
            "rejected": self.rejected,
            "timed_out": self.timed_out,
            "discarded_late": self.discarded_late,
            "peak_running": self.peak_running,
            "peak_queued": self.peak_queued,
        })
    }
}

/// How one call ended.
#[derive(Debug)]
pub enum Outcome<T> {
    Done(T),
    /// The work panicked (the payload is logged, never returned).
    Panicked,
    /// Queue full: nothing was started.
    Overloaded,
    /// The deadline passed (while queued or running); the work was
    /// cancelled.
    TimedOut,
}

#[derive(Default)]
struct State {
    limits: Option<Limits>,
    m: Metrics,
}

/// A bounded executor; cheap to share.
#[derive(Clone, Default)]
pub struct Executor {
    inner: Arc<(Mutex<State>, Condvar)>,
}

struct SlotGuard(Executor);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let (lock, cv) = &*self.0.inner;
        let mut s = lock.lock().unwrap_or_else(|p| p.into_inner());
        s.m.running -= 1;
        cv.notify_all();
    }
}

impl Executor {
    pub fn new(limits: Limits) -> Self {
        let e = Executor::default();
        e.set_limits(limits);
        e
    }

    /// Applies new limits (they take effect for the next admission).
    pub fn set_limits(&self, limits: Limits) {
        let (lock, cv) = &*self.inner;
        let mut s = lock.lock().unwrap_or_else(|p| p.into_inner());
        s.limits = Some(Limits {
            max_running: limits.max_running.max(1),
            max_queued: limits.max_queued,
        });
        cv.notify_all();
    }

    pub fn metrics(&self) -> Metrics {
        self.inner.0.lock().unwrap_or_else(|p| p.into_inner()).m
    }

    /// Runs `work` on a worker under `timeout`, queueing for a slot first.
    pub fn run<T, F>(&self, timeout: Duration, work: F) -> Outcome<T>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let token = Token::new(Some(timeout));
        let deadline = Instant::now() + timeout;
        let (lock, cv) = &*self.inner;
        // Admission: a free slot, else a queue place, else overloaded.
        {
            let mut s = lock.lock().unwrap_or_else(|p| p.into_inner());
            let limits = s.limits.unwrap_or(Limits {
                max_running: 1,
                max_queued: 0,
            });
            if s.m.running >= limits.max_running {
                if s.m.queued >= limits.max_queued {
                    s.m.rejected += 1;
                    return Outcome::Overloaded;
                }
                s.m.queued += 1;
                s.m.peak_queued = s.m.peak_queued.max(s.m.queued);
                loop {
                    let max_running = s.limits.map_or(1, |l| l.max_running);
                    if s.m.running < max_running {
                        break;
                    }
                    let now = Instant::now();
                    if now >= deadline {
                        s.m.queued -= 1;
                        s.m.timed_out += 1;
                        return Outcome::TimedOut;
                    }
                    s = cv
                        .wait_timeout(s, deadline - now)
                        .unwrap_or_else(|p| p.into_inner())
                        .0;
                }
                s.m.queued -= 1;
            }
            s.m.running += 1;
            s.m.peak_running = s.m.peak_running.max(s.m.running);
        }
        let slot = SlotGuard(self.clone());
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let worker_token = token.clone();
        let exec = self.clone();
        let spawned = std::thread::Builder::new()
            .name("docmonk-mcp-tool".into())
            .spawn(move || {
                let deadline_guard = worker_token.enter();
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
                drop(deadline_guard);
                // Free the slot before reporting, so a caller that sees the
                // result can immediately start another call.
                drop(slot);
                // The caller stopped waiting: the late result is discarded.
                if tx.send(outcome).is_err() {
                    let (lock, _) = &*exec.inner;
                    lock.lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .m
                        .discarded_late += 1;
                }
            });
        if spawned.is_err() {
            // The slot guard moved into the failed closure was dropped.
            return Outcome::Panicked;
        }
        let wait = deadline.saturating_duration_since(Instant::now());
        let outcome = match rx.recv_timeout(wait) {
            Ok(Ok(v)) => Outcome::Done(v),
            Ok(Err(payload)) => {
                let msg = payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                    .unwrap_or_default();
                eprintln!("docmonk mcp: tool worker panicked: {msg}");
                Outcome::Panicked
            }
            Err(_) => {
                token.cancel();
                let mut s = lock.lock().unwrap_or_else(|p| p.into_inner());
                s.m.timed_out += 1;
                drop(s);
                drop(rx);
                return Outcome::TimedOut;
            }
        };
        lock.lock().unwrap_or_else(|p| p.into_inner()).m.completed += 1;
        outcome
    }

    /// Waits up to `grace` for every running worker to stop (shutdown).
    pub fn drain(&self, grace: Duration) -> bool {
        let deadline = Instant::now() + grace;
        let (lock, cv) = &*self.inner;
        let mut s = lock.lock().unwrap_or_else(|p| p.into_inner());
        while s.m.running > 0 || s.m.queued > 0 {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            s = cv
                .wait_timeout(s, deadline - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        true
    }
}
