//! The process-wide resource governor (ADR 0033, P0-I02).
//!
//! Running several sources at once must not multiply per-source worker
//! pools beyond what the host has. Every source pass, worker and finalizer
//! in the process draws from the same bounded permit pools:
//!
//! | class        | bounds                                             |
//! |--------------|----------------------------------------------------|
//! | `bytes`      | estimated bytes of files queued or being processed |
//! | `ocr`        | heavy document conversions (PDF, scans, images)    |
//! | `embedding`  | embedding / reranker model batches                 |
//! | `cpu`        | any CPU-bound file processing                      |
//! | `io`         | server bulk/copy requests                          |
//!
//! **Lock order.** A thread only ever waits for a pool *after* every pool
//! it already holds, in the order `bytes < ocr < embedding < cpu < io`.
//! Holding a later pool while waiting for an earlier one never happens, so
//! permit waits cannot form a cycle. Source locks and server leases are
//! taken before any permit and released after all of them.
//!
//! Permits are RAII: dropping one (on success, error, early return or
//! panic unwinding) returns it. A request larger than a pool's capacity is
//! clamped to the capacity, so one oversized file runs alone instead of
//! waiting forever.

use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;

/// Permit classes, in acquisition order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    Bytes,
    Ocr,
    Embedding,
    Cpu,
    Io,
}

impl Class {
    pub const ALL: [Class; 5] = [
        Class::Bytes,
        Class::Ocr,
        Class::Embedding,
        Class::Cpu,
        Class::Io,
    ];

    fn index(self) -> usize {
        self as usize
    }
}

/// Capacities of every pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Limits {
    pub cpu: usize,
    pub ocr: usize,
    pub embedding: usize,
    pub io: usize,
    pub in_flight_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
        Self {
            cpu: cpus,
            ocr: 1,
            embedding: 1,
            io: 8,
            in_flight_bytes: 512 * 1024 * 1024,
        }
    }
}

impl Limits {
    pub fn from_config(c: &docmonk_config::model::IndexingConfig) -> Self {
        Self {
            cpu: c.resolved_cpu_workers().max(1),
            ocr: c.ocr_workers.max(1) as usize,
            embedding: c.embedding_workers.max(1) as usize,
            io: c.io_concurrency.max(1) as usize,
            in_flight_bytes: (c.max_in_flight_mb.max(1) as usize).saturating_mul(1024 * 1024),
        }
    }

    fn cap(&self, class: Class) -> usize {
        match class {
            Class::Bytes => self.in_flight_bytes,
            Class::Ocr => self.ocr,
            Class::Embedding => self.embedding,
            Class::Cpu => self.cpu,
            Class::Io => self.io,
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
pub struct PoolStats {
    pub capacity: usize,
    pub in_use: usize,
    pub peak: usize,
    pub waiting: usize,
    pub acquired: u64,
    pub completed: u64,
    pub wait_ms_total: u64,
    pub wait_ms_max: u64,
}

struct Pool {
    state: Mutex<PoolStats>,
    freed: Condvar,
}

impl Pool {
    fn new() -> Self {
        Self {
            state: Mutex::new(PoolStats::default()),
            freed: Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PoolStats> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// The governor: one bounded pool per [`Class`].
pub struct ResourceGovernor {
    pools: [Pool; 5],
}

/// A held share of one pool; returned on drop.
#[must_use = "a permit is released when dropped"]
pub struct Permit<'a> {
    pool: &'a Pool,
    class: Class,
    amount: usize,
}

impl Permit<'_> {
    pub fn class(&self) -> Class {
        self.class
    }
    pub fn amount(&self) -> usize {
        self.amount
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut s = self.pool.lock();
        s.in_use = s.in_use.saturating_sub(self.amount);
        s.completed += 1;
        drop(s);
        self.pool.freed.notify_all();
    }
}

impl Default for ResourceGovernor {
    fn default() -> Self {
        Self::new(Limits::default())
    }
}

impl ResourceGovernor {
    pub fn new(limits: Limits) -> Self {
        let g = Self {
            pools: [
                Pool::new(),
                Pool::new(),
                Pool::new(),
                Pool::new(),
                Pool::new(),
            ],
        };
        g.set_limits(limits);
        g
    }

    /// Changes capacities. Permits already held stay valid; new requests
    /// see the new capacity.
    pub fn set_limits(&self, limits: Limits) {
        for class in Class::ALL {
            let pool = &self.pools[class.index()];
            pool.lock().capacity = limits.cap(class).max(1);
            pool.freed.notify_all();
        }
    }

    /// Blocks until `amount` units of `class` are free (clamped to the
    /// pool's capacity) and returns them as a permit.
    pub fn acquire(&self, class: Class, amount: usize) -> Permit<'_> {
        let pool = &self.pools[class.index()];
        let started = Instant::now();
        let mut s = pool.lock();
        let want = amount.clamp(1, s.capacity.max(1));
        s.waiting += 1;
        while s.in_use + want > s.capacity && s.in_use > 0 {
            s = pool.freed.wait(s).unwrap_or_else(|p| p.into_inner());
        }
        s.waiting -= 1;
        s.in_use += want;
        s.peak = s.peak.max(s.in_use);
        s.acquired += 1;
        let waited = started.elapsed().as_millis() as u64;
        s.wait_ms_total += waited;
        s.wait_ms_max = s.wait_ms_max.max(waited);
        Permit {
            pool,
            class,
            amount: want,
        }
    }

    /// [`Self::acquire`], giving up after `timeout`.
    pub fn try_acquire_for(
        &self,
        class: Class,
        amount: usize,
        timeout: Duration,
    ) -> Option<Permit<'_>> {
        let pool = &self.pools[class.index()];
        let deadline = Instant::now() + timeout;
        let mut s = pool.lock();
        let want = amount.clamp(1, s.capacity.max(1));
        while s.in_use + want > s.capacity && s.in_use > 0 {
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            s = pool
                .freed
                .wait_timeout(s, deadline - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        s.in_use += want;
        s.peak = s.peak.max(s.in_use);
        s.acquired += 1;
        Some(Permit {
            pool,
            class,
            amount: want,
        })
    }

    pub fn stats(&self, class: Class) -> PoolStats {
        self.pools[class.index()].lock().clone()
    }

    /// Every pool's counters, keyed by class name (status JSON).
    pub fn snapshot(&self) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        for class in Class::ALL {
            let name = serde_json::to_value(class)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default();
            m.insert(
                name,
                serde_json::to_value(self.stats(class)).unwrap_or_default(),
            );
        }
        serde_json::Value::Object(m)
    }
}

static GLOBAL: OnceLock<ResourceGovernor> = OnceLock::new();

/// The process's governor (default limits until [`configure`]).
pub fn global() -> &'static ResourceGovernor {
    GLOBAL.get_or_init(ResourceGovernor::default)
}

/// Applies `limits` to the process's governor.
pub fn configure(limits: Limits) -> &'static ResourceGovernor {
    let g = global();
    g.set_limits(limits);
    g
}

/// The class a file's processing draws from besides `cpu`: heavy document
/// formats (PDF, scans, images) also take an `ocr` permit.
pub fn heavy_document(rel_path: &str) -> bool {
    let ext = rel_path
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "pdf" | "png" | "jpg" | "jpeg" | "tif" | "tiff" | "bmp" | "gif" | "webp" | "djvu"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn small() -> Limits {
        Limits {
            cpu: 2,
            ocr: 1,
            embedding: 1,
            io: 2,
            in_flight_bytes: 100,
        }
    }

    #[test]
    fn caps_are_never_exceeded_under_contention() {
        let g = Arc::new(ResourceGovernor::new(small()));
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        std::thread::scope(|s| {
            for _ in 0..32 {
                let (g, live, peak) = (g.clone(), live.clone(), peak.clone());
                s.spawn(move || {
                    for _ in 0..20 {
                        let _p = g.acquire(Class::Cpu, 1);
                        let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        std::thread::yield_now();
                        live.fetch_sub(1, Ordering::SeqCst);
                    }
                });
            }
        });
        assert!(peak.load(Ordering::SeqCst) <= 2);
        let st = g.stats(Class::Cpu);
        assert_eq!(st.in_use, 0, "every permit returned");
        assert!(st.peak <= 2);
        assert_eq!(st.acquired, 640);
        assert_eq!(st.completed, 640);
    }

    #[test]
    fn byte_budget_is_weighted_and_oversized_requests_run_alone() {
        let g = ResourceGovernor::new(small());
        let a = g.acquire(Class::Bytes, 60);
        assert!(g
            .try_acquire_for(Class::Bytes, 60, Duration::from_millis(20))
            .is_none());
        let b = g.acquire(Class::Bytes, 40);
        assert_eq!(g.stats(Class::Bytes).in_use, 100);
        drop((a, b));
        // 10 GB clamps to the 100-byte budget and is granted when idle.
        let big = g.acquire(Class::Bytes, 10_000_000_000);
        assert_eq!(big.amount(), 100);
        drop(big);
        assert_eq!(g.stats(Class::Bytes).in_use, 0);
    }

    #[test]
    fn permits_return_on_panic_unwind() {
        let g = ResourceGovernor::new(small());
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _p = g.acquire(Class::Ocr, 1);
            panic!("boom");
        }));
        assert!(r.is_err());
        assert_eq!(g.stats(Class::Ocr).in_use, 0);
        assert!(g
            .try_acquire_for(Class::Ocr, 1, Duration::from_millis(10))
            .is_some());
    }

    /// Cheap work keeps flowing while the only OCR permit is held by a
    /// long-running heavy document.
    #[test]
    fn cheap_work_progresses_while_ocr_is_saturated() {
        let g = Arc::new(ResourceGovernor::new(small()));
        let ocr = g.acquire(Class::Ocr, 1);
        let done = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..4 {
                let g = g.clone();
                let done = &done;
                s.spawn(move || {
                    for _ in 0..10 {
                        let _c = g.acquire(Class::Cpu, 1);
                        done.fetch_add(1, Ordering::SeqCst);
                    }
                });
            }
        });
        assert_eq!(done.load(Ordering::SeqCst), 40);
        drop(ocr);
    }

    #[test]
    fn ordered_acquisition_has_no_cycles() {
        // Many threads taking bytes -> ocr -> cpu -> io in order, with
        // capacities of one, always finish.
        let g = Arc::new(ResourceGovernor::new(Limits {
            cpu: 1,
            ocr: 1,
            embedding: 1,
            io: 1,
            in_flight_bytes: 1,
        }));
        std::thread::scope(|s| {
            for t in 0..8 {
                let g = g.clone();
                s.spawn(move || {
                    for _ in 0..25 {
                        let _b = g.acquire(Class::Bytes, 1);
                        let _o = (t % 2 == 0).then(|| g.acquire(Class::Ocr, 1));
                        let _c = g.acquire(Class::Cpu, 1);
                        let _i = g.acquire(Class::Io, 1);
                    }
                });
            }
        });
        for c in Class::ALL {
            assert_eq!(g.stats(c).in_use, 0);
        }
    }

    #[test]
    fn heavy_formats() {
        assert!(heavy_document("a/b/scan.PDF"));
        assert!(heavy_document("x.tiff"));
        assert!(!heavy_document("src/Main.cs"));
        assert!(!heavy_document("notes.md"));
    }
}
