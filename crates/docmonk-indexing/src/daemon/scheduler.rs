//! The daemon's trigger coalescing, without threads
//! (`Daemon.enqueue_source`, `_build_scan_request` and
//! `_settle_pending_state`). Each source is in one of four states: no
//! entry, `queued`, `running`, or `running_followup`. A burst of triggers
//! becomes at most one queued pass plus one follow-up. Touched paths,
//! force-full and the first reason accumulate until the next pass takes
//! them.
//!
//! **Fairness (ADR 0033).** The queue holds at most one entry per source.
//! With [`Policy::urgent_first`], a small watcher-targeted pass is started
//! before full reconciliation passes, but no entry ever waits longer than
//! [`Policy::max_wait`] behind urgent work: an entry that has waited that
//! long is started first (oldest first). Every continuously eligible
//! source is therefore started within `max_wait` plus the passes already
//! running. The default policy is plain FIFO.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::{ScanRequest, FORCE_FULL_REASONS, MAX_TARGETED_PATHS};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Queued,
    Running,
    RunningFollowup,
}

impl Pending {
    fn as_str(self) -> &'static str {
        match self {
            Pending::Queued => "queued",
            Pending::Running => "running",
            Pending::RunningFollowup => "running_followup",
        }
    }
}

/// How the next pass is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// Prefer small watcher-targeted passes over full passes.
    pub urgent_first: bool,
    /// Longest any queued entry waits behind urgent work.
    pub max_wait: Duration,
    /// Touched paths kept per source before the pass becomes a full one.
    pub max_paths: usize,
}

impl Default for Policy {
    /// Plain FIFO.
    fn default() -> Self {
        Self {
            urgent_first: false,
            max_wait: Duration::from_secs(120),
            max_paths: MAX_TARGETED_PATHS,
        }
    }
}

#[derive(Debug, Default)]
pub struct Scheduler {
    queue: VecDeque<String>,
    pending: HashMap<String, Pending>,
    touched: HashMap<String, BTreeSet<PathBuf>>,
    force_full: HashMap<String, bool>,
    reasons: HashMap<String, String>,
    enqueued_at: HashMap<String, Instant>,
    policy: Policy,
}

impl Scheduler {
    pub fn with_policy(policy: Policy) -> Self {
        Self {
            policy,
            ..Self::default()
        }
    }

    fn max_paths(&self) -> usize {
        if self.policy.max_paths == 0 {
            MAX_TARGETED_PATHS
        } else {
            self.policy.max_paths
        }
    }

    /// Whether the queued entry for `id` is a small targeted pass.
    fn urgent(&self, id: &str) -> bool {
        !self.force_full.get(id).copied().unwrap_or(false)
            && self.touched.get(id).is_some_and(|t| !t.is_empty())
    }

    /// Longest current queue wait (status/observability).
    pub fn oldest_wait(&self, now: Instant) -> Option<Duration> {
        self.queue
            .iter()
            .filter_map(|id| self.enqueued_at.get(id))
            .map(|t| now.saturating_duration_since(*t))
            .max()
    }

    /// Records a trigger. True when it put a new entry on the queue.
    pub fn enqueue(&mut self, source_id: &str, reason: &str, paths: &[PathBuf]) -> bool {
        self.enqueue_at(source_id, reason, paths, Instant::now())
    }

    /// [`Self::enqueue`] at `now` (deterministic tests).
    pub fn enqueue_at(
        &mut self,
        source_id: &str,
        reason: &str,
        paths: &[PathBuf],
        now: Instant,
    ) -> bool {
        if FORCE_FULL_REASONS.contains(&reason) {
            self.force_full.insert(source_id.into(), true);
        }
        // Memory stays bounded however many events a burst delivers: past
        // MAX_TARGETED_PATHS the next pass is a full one anyway, so the
        // paths are dropped instead of accumulated.
        if !paths.is_empty() && !self.force_full.get(source_id).copied().unwrap_or(false) {
            let set = self.touched.entry(source_id.into()).or_default();
            set.extend(paths.iter().cloned());
            if set.len() > self.max_paths() {
                self.touched.remove(source_id);
                self.force_full.insert(source_id.into(), true);
            }
        }
        self.reasons
            .entry(source_id.into())
            .or_insert_with(|| reason.into());
        match self.pending.get(source_id).copied() {
            None => {
                self.pending.insert(source_id.into(), Pending::Queued);
                self.queue.push_back(source_id.into());
                self.enqueued_at.insert(source_id.into(), now);
                true
            }
            Some(Pending::Running) => {
                self.pending
                    .insert(source_id.into(), Pending::RunningFollowup);
                false
            }
            Some(Pending::Queued | Pending::RunningFollowup) => false,
        }
    }

    /// Pops the next source (by [`Policy`]) and marks it running.
    pub fn start_next(&mut self) -> Option<String> {
        self.start_next_at(Instant::now())
    }

    /// [`Self::start_next`] at `now` (deterministic tests).
    pub fn start_next_at(&mut self, now: Instant) -> Option<String> {
        let pos = if self.policy.urgent_first {
            let waited = |id: &String| {
                self.enqueued_at
                    .get(id)
                    .map_or(Duration::ZERO, |t| now.saturating_duration_since(*t))
            };
            // 1. Anything that waited max_wait, oldest first (aging).
            self.queue
                .iter()
                .enumerate()
                .filter(|(_, id)| waited(id) >= self.policy.max_wait)
                .max_by_key(|(i, id)| (waited(id), std::cmp::Reverse(*i)))
                .map(|(i, _)| i)
                // 2. Else the first urgent (small targeted) entry.
                .or_else(|| self.queue.iter().position(|id| self.urgent(id)))
                // 3. Else FIFO.
                .unwrap_or(0)
        } else {
            0
        };
        let id = self.queue.remove(pos)?;
        self.enqueued_at.remove(&id);
        self.pending.insert(id.clone(), Pending::Running);
        Some(id)
    }

    /// Takes what accumulated for `source_id`: the next pass's request.
    pub fn take_request(&mut self, source_id: &str) -> ScanRequest {
        let touched = self.touched.remove(source_id).unwrap_or_default();
        let force_full = self.force_full.remove(source_id).unwrap_or(false);
        let reason = self
            .reasons
            .remove(source_id)
            .unwrap_or_else(|| "daemon".into());
        let full = force_full || touched.is_empty() || touched.len() > self.max_paths();
        ScanRequest {
            source_id: source_id.into(),
            reason,
            full,
            changed_paths: if full { BTreeSet::new() } else { touched },
        }
    }

    /// A pass finished. A follow-up, if one arrived, goes to the back of
    /// the queue (true).
    pub fn settle(&mut self, source_id: &str) -> bool {
        self.settle_at(source_id, Instant::now())
    }

    /// [`Self::settle`] at `now` (deterministic tests).
    pub fn settle_at(&mut self, source_id: &str, now: Instant) -> bool {
        if self.pending.remove(source_id) == Some(Pending::RunningFollowup) {
            self.pending.insert(source_id.into(), Pending::Queued);
            self.queue.push_back(source_id.into());
            self.enqueued_at.insert(source_id.into(), now);
            return true;
        }
        false
    }

    pub fn has_queued(&self) -> bool {
        !self.queue.is_empty()
    }

    pub fn queue(&self) -> Vec<String> {
        self.queue.iter().cloned().collect()
    }

    /// The documented state name, if any.
    pub fn state_of(&self, source_id: &str) -> Option<&'static str> {
        self.pending.get(source_id).map(|p| p.as_str())
    }

    pub fn is_idle(&self) -> bool {
        self.queue.is_empty() && self.pending.is_empty()
    }

    /// Drops everything queued (shutdown).
    pub fn clear_queue(&mut self) {
        for id in self.queue.drain(..) {
            self.pending.remove(&id);
            self.enqueued_at.remove(&id);
        }
    }

    /// Sources currently running.
    pub fn running(&self) -> usize {
        self.pending
            .values()
            .filter(|p| matches!(p, Pending::Running | Pending::RunningFollowup))
            .count()
    }
}

#[cfg(test)]
mod bound_tests {
    use super::*;

    #[test]
    fn a_huge_burst_becomes_one_full_pass_without_keeping_paths() {
        let mut s = Scheduler::default();
        for i in 0..(MAX_TARGETED_PATHS * 3) {
            s.enqueue("src", "local_watcher", &[PathBuf::from(format!("f{i}"))]);
        }
        assert!(
            !s.touched.contains_key("src"),
            "paths are not retained past the cap"
        );
        assert_eq!(s.queue(), ["src"]);
        assert_eq!(s.start_next().as_deref(), Some("src"));
        let req = s.take_request("src");
        assert!(req.full);
        assert!(req.changed_paths.is_empty());
    }

    #[test]
    fn small_bursts_stay_targeted_and_each_source_is_queued_once() {
        let mut s = Scheduler::default();
        for i in 0..10 {
            s.enqueue("a", "local_watcher", &[PathBuf::from(format!("f{i}"))]);
            s.enqueue("b", "local_watcher", &[PathBuf::from(format!("g{i}"))]);
        }
        assert_eq!(s.queue(), ["a", "b"], "FIFO, one entry per source");
        s.start_next();
        let req = s.take_request("a");
        assert!(!req.full);
        assert_eq!(req.changed_paths.len(), 10);
    }
}

#[cfg(test)]
mod fairness_tests {
    use super::*;

    fn fair(max_wait_s: u64) -> Scheduler {
        Scheduler::with_policy(Policy {
            urgent_first: true,
            max_wait: Duration::from_secs(max_wait_s),
            max_paths: 50,
        })
    }

    #[test]
    fn urgent_targeted_passes_go_first() {
        let mut s = fair(120);
        s.enqueue("big", "reconciliation", &[]);
        s.enqueue("small", "local_watcher", &[PathBuf::from("a.cs")]);
        assert_eq!(s.start_next().as_deref(), Some("small"));
        assert_eq!(s.start_next().as_deref(), Some("big"));
    }

    /// Simulation: one source is edited continuously while three others
    /// only need full passes. Every source is started within max_wait.
    #[test]
    fn aging_prevents_starvation_under_sustained_urgent_load() {
        let mut s = fair(10);
        let t0 = Instant::now();
        for id in ["r1", "r2", "r3"] {
            s.enqueue_at(id, "reconciliation", &[], t0);
        }
        let mut started_at: HashMap<String, u64> = HashMap::new();
        // Each simulated second: "hot" gets an edit, one pass runs.
        for sec in 0..40u64 {
            let now = t0 + Duration::from_secs(sec);
            s.enqueue_at(
                "hot",
                "local_watcher",
                &[PathBuf::from(format!("f{sec}"))],
                now,
            );
            let id = s.start_next_at(now).unwrap();
            started_at.entry(id.clone()).or_insert(sec);
            s.take_request(&id);
            s.settle_at(&id, now);
            if id != "hot" {
                // Reconciliation re-queues it later; keep it eligible.
                s.enqueue_at(&id, "reconciliation", &[], now);
            }
        }
        for id in ["r1", "r2", "r3"] {
            let at = started_at.get(id).copied();
            assert!(at.is_some_and(|t| t <= 12), "{id} started at {at:?}");
        }
        assert!(started_at.contains_key("hot"));
    }

    #[test]
    fn at_most_one_queued_follow_up_per_source() {
        let mut s = fair(120);
        s.enqueue("a", "local_watcher", &[PathBuf::from("x")]);
        assert_eq!(s.start_next().as_deref(), Some("a"));
        for i in 0..100 {
            s.enqueue("a", "local_watcher", &[PathBuf::from(format!("y{i}"))]);
        }
        assert!(s.queue().is_empty(), "follow-up waits for the running pass");
        assert!(s.settle("a"));
        assert_eq!(s.queue(), ["a"]);
        let req = {
            s.start_next();
            s.take_request("a")
        };
        // 100 paths > max_paths (50): promoted to a full pass, paths dropped.
        assert!(req.full);
        assert!(req.changed_paths.is_empty());
    }
}
