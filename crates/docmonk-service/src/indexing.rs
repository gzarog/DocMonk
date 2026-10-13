//! Indexing runs over registered sources.
//!
//! A run is two-staged with an explicit barrier: phase 1 indexes and
//! publishes every selected source; only after **all** phase-1 passes
//! ended does phase 2 ([`crate::relationships`]) build relationship graphs
//! for the sources that published, when `indexing.relationships_enabled`
//! is on. Search over a newly published base index is available as soon as
//! its phase-1 pass publishes; a graph failure never rolls it back and is
//! reported separately from indexing failures.
//!
//! Up to `indexing.max_parallel_sources` independent sources run at once
//! (ADR 0033). Each source pass holds its own
//! `locks/index-<source_id>.lock` (and, in server mode, the source's
//! server writer lease), so one source never runs twice at the same time,
//! while different sources never wait for each other. A source that cannot
//! be locked, fails, or is unreachable is reported and the others carry on.
//! Whole-home operations (backup, restore, vector maintenance) take
//! [`home_exclusive`]: `locks/index.lock` and then every source lock in id
//! order, so they never interleave with a pass and locks are always taken
//! in one global order (no lock inversion).
//!
//! Events are delivered on the calling thread, in completion order, through
//! a bounded channel. Progress is published to the home's progress file.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use docmonk_core::errors::{ErrorKind, RagMonkError};
use docmonk_core::paths::Home;
use docmonk_indexing::coordinator::{run_source_with, Options, SourceResult};
use docmonk_indexing::lock::RunLock;
use docmonk_indexing::progress::ProgressTracker;
use docmonk_storage::control::SourceRecord;
use docmonk_storage::StorageLayout;

use crate::backend::Backend;
use crate::sources::{catalog, control_plane};
use crate::{db, load};

/// What happened to one source during a run.
pub enum SourceEvent<'a> {
    /// The source is next; its lock has not been taken yet.
    Started { source: &'a SourceRecord },
    /// Another process holds the source's lock (or server writer lease).
    Blocked {
        source: &'a SourceRecord,
        error: &'a RagMonkError,
    },
    /// The pass failed before completing.
    Failed {
        source: &'a SourceRecord,
        error: &'a RagMonkError,
    },
    /// The pass ran; `run` says what it did (including offline/incomplete).
    Completed {
        source: &'a SourceRecord,
        run: &'a SourceResult,
    },
    /// Phase 2 ended for a source (after every phase-1 pass of the run).
    Relationships {
        source: &'a SourceRecord,
        outcome: &'a crate::relationships::RelationshipOutcome,
    },
    /// The cross-source dependency stage ended for a consumer source (one
    /// in this run or one impacted by it).
    Dependencies {
        source: &'a SourceRecord,
        outcome: &'a crate::dependencies::DependencyOutcome,
    },
}

/// The outcome of a whole run, with its concurrency metrics.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct RunSummary {
    /// Identifies this run in progress, logs and status.
    pub run_id: String,
    pub attempted: usize,
    pub failed_sources: usize,
    pub failed_files: usize,
    /// Files left for a later retry (transient failures).
    pub retrying_files: usize,
    /// Longest wait for a source's lock.
    pub max_lock_wait_seconds: f64,
    /// Highest pipeline in-flight count and active-worker count seen.
    pub in_flight_high_water: usize,
    pub active_workers_high_water: usize,
    /// Most source passes running at the same time, and the configured cap.
    pub parallel_sources_high_water: usize,
    pub max_parallel_sources: usize,
    /// `indexing.relationships_enabled` for this run.
    pub relationships_enabled: bool,
    /// Phase 2 totals (all zero when relationships are disabled).
    pub relationships: crate::relationships::StageSummary,
    /// Cross-source dependency stage totals.
    pub dependencies: crate::dependencies::DependencySummary,
}

impl RunSummary {
    /// `success`, `partial_success` (every index published, some
    /// relationship graph not) or `failed` (an indexing failure).
    pub fn outcome(&self) -> &'static str {
        if self.failed_sources > 0 || self.failed_files > 0 {
            "failed"
        } else if self.relationships.not_current() > 0 {
            "partial_success"
        } else {
            "success"
        }
    }

    /// An `IndexingPartialFailure` error when anything failed. Indexing
    /// failures and relationship failures are reported separately.
    pub fn into_result(self) -> Result<Self, RagMonkError> {
        let graph = self.relationships.not_current();
        if self.failed_sources == 0 && self.failed_files == 0 && graph == 0 {
            return Ok(self);
        }
        let mut parts = Vec::new();
        if self.failed_sources > 0 {
            parts.push(format!("{} source(s) failed", self.failed_sources));
        }
        if self.failed_files > 0 {
            parts.push(format!("{} file(s) failed to index", self.failed_files));
        }
        if graph > 0 {
            parts.push(format!(
                "relationships were not published for {graph} source(s) (their index is published and searchable; retry with 'ragmonk relationships build')"
            ));
        }
        Err(RagMonkError::new(
            ErrorKind::IndexingPartialFailure,
            format!("{}; see 'ragmonk doctor' for details", parts.join("; ")),
        ))
    }
}

/// Run-level options beyond the configuration.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Rebuild every selected source from scratch (`ragmonk rebuild`).
    pub force_full: bool,
    /// Only examine these absolute paths (a watcher-triggered pass).
    pub targets: Option<std::collections::BTreeSet<PathBuf>>,
    /// Override `indexing.max_parallel_sources`.
    pub max_parallel_sources: Option<usize>,
    /// The run this pass belongs to (set by [`index_sources_with`]).
    pub run_id: Option<String>,
}

/// `locks/index-<source_id>.lock`: one pass per source at a time.
pub fn source_lock_path(home: &Home, source_id: &str) -> PathBuf {
    docmonk_indexing::lock::source_lock_path(home, source_id)
}

/// The server writer-lease owner of this process (`host:pid`).
pub fn lease_owner() -> String {
    format!(
        "{}:{}",
        docmonk_indexing::runtime::host_name(),
        std::process::id()
    )
}

/// A new run id: `run-<utc>-<pid>`.
pub fn new_run_id() -> String {
    format!(
        "run-{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ"),
        std::process::id()
    )
}

/// Every lock a whole-home operation needs, held until dropped.
pub struct HomeLocks {
    locks: Vec<RunLock>,
}

impl HomeLocks {
    pub fn release(self) {
        for l in self.locks.into_iter().rev() {
            l.release();
        }
    }
}

/// `locks/index.lock`, then every registered source's lock in id order.
pub fn home_exclusive(home: &Home, operation: &str) -> Result<HomeLocks, RagMonkError> {
    let cfg = load(home)?;
    let timeout = Duration::from_secs_f64(cfg.indexing.lock_timeout_seconds);
    let mut locks = vec![RunLock::acquire(
        &home.locks_dir().join("index.lock"),
        operation,
        None,
        timeout,
    )?];
    let mut ids: Vec<String> = match catalog(home) {
        Ok(c) => c.list(false)?.into_iter().map(|s| s.id).collect(),
        Err(e) => {
            for l in locks {
                l.release();
            }
            return Err(e);
        }
    };
    ids.sort();
    for id in ids {
        match RunLock::acquire(&source_lock_path(home, &id), operation, Some(&id), timeout) {
            Ok(l) => locks.push(l),
            Err(e) => {
                HomeLocks { locks }.release();
                return Err(e);
            }
        }
    }
    Ok(HomeLocks { locks })
}

/// The sources a run covers: one by id, or every enabled source.
pub fn selected_sources(
    home: &Home,
    source_id: Option<&str>,
) -> Result<Vec<SourceRecord>, RagMonkError> {
    let c = catalog(home)?;
    match source_id {
        Some(id) => Ok(vec![c.get(id)?]),
        None => c.list(true),
    }
}

/// Indexes `sources`, reporting each through `on_event`.
pub fn index_sources(
    home: &Home,
    sources: &[SourceRecord],
    operation: &str,
    on_event: impl FnMut(SourceEvent<'_>),
) -> Result<RunSummary, RagMonkError> {
    index_sources_with(home, sources, operation, &RunOptions::default(), on_event)
}

/// What a worker reports to the calling thread.
enum Outcome {
    Started,
    Blocked(RagMonkError),
    Failed(RagMonkError),
    Completed(Box<SourceResult>),
}

/// [`index_sources`] with run options: up to `max_parallel_sources`
/// sources at once, each under its own lock.
pub fn index_sources_with(
    home: &Home,
    sources: &[SourceRecord],
    operation: &str,
    run: &RunOptions,
    mut on_event: impl FnMut(SourceEvent<'_>),
) -> Result<RunSummary, RagMonkError> {
    let cfg = load(home)?;
    let backend = crate::backend::open_for_write(home)?;
    let registry =
        docmonk_convert::registry_with(&cfg, &docmonk_convert::RegistryOptions::for_home(home));
    let opts = Options::from_config(&cfg);
    let governor = docmonk_indexing::governor::configure(
        docmonk_indexing::governor::Limits::from_config(&cfg.indexing),
    );
    let cap = run
        .max_parallel_sources
        .unwrap_or_else(|| cfg.indexing.resolved_max_parallel_sources())
        .max(1);
    let parallel = cap.min(sources.len()).max(1);
    let mut summary = RunSummary {
        run_id: new_run_id(),
        attempted: sources.len(),
        max_parallel_sources: cap,
        ..RunSummary::default()
    };
    let total = sources.len() as i64;
    let lock_timeout = opts.lock_timeout;
    let run_id = summary.run_id.clone();
    let run = &RunOptions {
        run_id: Some(run_id.clone()),
        ..run.clone()
    };
    docmonk_indexing::progress::track(&home.index_progress(), operation, Some(total), |tracker| {
        tracker.set_run(&run_id, parallel);
        tracker.set_phase("indexing");
        let lock_wait_ms = AtomicUsize::new(0);
        // Phase-1 passes that published (or confirmed) a base index.
        let mut indexed: Vec<usize> = Vec::new();
        let stats = docmonk_indexing::executor::run_bounded(
            sources,
            parallel,
            |source, emit| {
                emit.emit(Outcome::Started);
                let waiting = Instant::now();
                let lock = RunLock::acquire(
                    &source_lock_path(home, &source.id),
                    operation,
                    Some(&source.id),
                    lock_timeout,
                );
                lock_wait_ms.fetch_max(waiting.elapsed().as_millis() as usize, Ordering::SeqCst);
                let outcome = match lock {
                    Err(e) => Outcome::Blocked(e),
                    Ok(lock) => {
                        tracker.source_started(&source.id);
                        let mut progress = tracker.clone();
                        // The lock guard is released on drop, so a panicking
                        // pass (reported by the executor) never leaks it.
                        let r = run_one(
                            home,
                            &cfg,
                            &backend,
                            source,
                            &registry,
                            &opts,
                            run,
                            &mut progress,
                        );
                        tracker.source_finished(&source.id, r.is_ok());
                        lock.release();
                        match r {
                            Ok(res) => Outcome::Completed(Box::new(res)),
                            Err(e) if e.class() == Some("BackendConflictError") => {
                                Outcome::Blocked(e)
                            }
                            Err(e) => Outcome::Failed(e),
                        }
                    }
                };
                emit.emit(outcome);
            },
            |_| {
                Outcome::Failed(RagMonkError::new(
                    ErrorKind::Generic,
                    "source pass panicked; its pending build was discarded",
                ))
            },
            |i, outcome| {
                let source = &sources[i];
                match outcome {
                    Outcome::Started => on_event(SourceEvent::Started { source }),
                    Outcome::Blocked(error) => {
                        summary.failed_sources += 1;
                        on_event(SourceEvent::Blocked {
                            source,
                            error: &error,
                        });
                    }
                    Outcome::Failed(error) => {
                        summary.failed_sources += 1;
                        tracing::warn!(component = "indexing", event = "source_failed", run_id = %summary.run_id, source_id = %source.id, error = %docmonk_telemetry::redact::redact_urls_in_text(error.message()));
                        on_event(SourceEvent::Failed {
                            source,
                            error: &error,
                        });
                    }
                    Outcome::Completed(run) => {
                        if run.offline.is_none() {
                            summary.failed_files += run.failed;
                            summary.retrying_files += run.retrying;
                            if run.build_id.is_some() {
                                indexed.push(i);
                            }
                        }
                        summary.in_flight_high_water = summary
                            .in_flight_high_water
                            .max(run.pipeline.in_flight_high_water);
                        summary.active_workers_high_water = summary
                            .active_workers_high_water
                            .max(run.pipeline.active_workers_high_water);
                        on_event(SourceEvent::Completed { source, run: &run });
                    }
                }
            },
        );
        summary.parallel_sources_high_water = stats.running_high_water;
        summary.max_lock_wait_seconds = lock_wait_ms.load(Ordering::SeqCst) as f64 / 1000.0;
        // The barrier: every phase-1 pass of this batch has ended.
        tracker.set_phase("indexed");
        indexed.sort_unstable();
        let ready: Vec<SourceRecord> = indexed.iter().map(|i| sources[*i].clone()).collect();
        summary.relationships_enabled = cfg.indexing.relationships_enabled;
        if cfg.indexing.relationships_enabled {
            for s in &ready {
                tracker.source_relationships(&s.id, "pending", None, None);
            }
            tracker.set_phase("relationships");
            crate::relationships::run_stage(
                home,
                &cfg,
                &backend,
                &ready,
                operation,
                parallel,
                &opts,
                Some(tracker),
                |source, outcome| {
                    summary.relationships.add(&outcome);
                    on_event(SourceEvent::Relationships {
                        source,
                        outcome: &outcome,
                    });
                },
            );
            // Stage 3: consumers of this run's sources (selected or not)
            // are updated against their existing published bases.
            match crate::sources::catalog(home).and_then(|c| c.list(false)) {
                Ok(all) => crate::dependencies::run_stage(
                    home,
                    &cfg,
                    &backend,
                    &all,
                    opts.lock_timeout,
                    |source, outcome| {
                        summary.dependencies.add(&outcome);
                        on_event(SourceEvent::Dependencies {
                            source,
                            outcome: &outcome,
                        });
                    },
                ),
                Err(e) => {
                    tracing::warn!(component = "relationships", event = "dependencies_skipped", error = %e.message())
                }
            }
        } else {
            // No graph jobs; existing graphs are hidden, never deleted.
            for s in &ready {
                if let Err(e) = crate::relationships::mark_disabled(home, &cfg, &backend, s) {
                    tracing::debug!(component = "relationships", event = "disable_not_recorded", source_id = %s.id, error = %e.message());
                }
                tracker.source_relationships(&s.id, "disabled", None, None);
            }
        }
        tracker.set_phase("done");
        tracker.set_resources(&governor.snapshot());
        Ok::<(), RagMonkError>(())
    })?;
    Ok(summary)
}

/// One source pass against the configured backend. The caller holds the
/// source's lock.
#[allow(clippy::too_many_arguments)]
fn run_one(
    home: &Home,
    cfg: &docmonk_config::RagMonkConfig,
    backend: &Backend,
    source: &SourceRecord,
    registry: &docmonk_indexing::coordinator::Registry,
    opts: &Options,
    run: &RunOptions,
    progress: &mut ProgressTracker,
) -> Result<SourceResult, RagMonkError> {
    match backend {
        Backend::Local => {
            // Each worker owns its connection (rusqlite connections are not
            // shared between threads).
            let mut cp = control_plane(home)?;
            if run.force_full {
                cp.require_full_rebuild(&source.id, "manual rebuild")
                    .map_err(db)?;
            }
            run_source_with(
                &StorageLayout::new(home),
                &mut cp,
                source,
                registry,
                opts,
                run.targets.as_ref(),
                progress,
            )
        }
        Backend::Server(server) => crate::server_index::run_source(
            home, cfg, server, source, registry, opts, run, progress,
        ),
    }
}
