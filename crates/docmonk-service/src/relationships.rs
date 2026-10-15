//! The relationship graph stage of a run (phase 2).
//!
//! `docmonk index` (and every automatic daemon pass) is two-staged per
//! source: phase 1 indexes and publishes the source ([`crate::indexing`]),
//! and as soon as that source's base index is published phase 2 derives
//! and publishes its relationship graph (when missing, stale or failed),
//! while other sources of the batch may still be indexing; the CPU
//! governor bounds parsing across all of them. Phase 2 never
//! runs when `indexing.relationships_enabled` is off, never holds back or
//! rolls back a base publication, and one source's graph failure never
//! stops another's. `docmonk relationships build` runs phase 2 alone, to
//! retry or initialize graphs without reindexing any content.

use std::path::Path;

use docmonk_code::graph_stage::{GraphError, GraphOutcome, GraphReport};
use docmonk_core::errors::{DocMonkError, ErrorKind};
use docmonk_core::paths::{project_id_for_canonical, Home};
use docmonk_indexing::coordinator::Options;
use docmonk_indexing::lock::RunLock;
use docmonk_indexing::progress::ProgressTracker;
use docmonk_storage::control::SourceRecord;
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::StorageLayout;
use serde::Serialize;

use crate::backend::Backend;
use crate::indexing::source_lock_path;
use crate::server_index::{ServerGraphError, ServerGraphRun};
use crate::sources::control_plane;
use crate::{db, load};

/// What phase 2 did for one source.
#[derive(Debug)]
pub enum RelationshipOutcome {
    /// A new graph generation was published.
    Built(Box<GraphRun>),
    /// The published graph already matched the active base.
    UpToDate(Box<GraphRun>),
    /// The inputs no longer match the published base; retried after the
    /// next index pass.
    Stale(String),
    /// The graph build failed; the base index stays published.
    Failed(DocMonkError),
    /// The source's lock or lease is held elsewhere.
    Blocked(DocMonkError),
    /// Nothing to build from (no published base yet).
    Skipped(String),
}

impl RelationshipOutcome {
    /// `state` as recorded in progress and summaries.
    pub fn state(&self) -> &'static str {
        match self {
            RelationshipOutcome::Built(_) => "ready",
            RelationshipOutcome::UpToDate(_) => "up_to_date",
            RelationshipOutcome::Stale(_) => "stale",
            RelationshipOutcome::Failed(_) | RelationshipOutcome::Blocked(_) => "failed",
            RelationshipOutcome::Skipped(_) => "skipped",
        }
    }

    pub fn error(&self) -> Option<String> {
        match self {
            RelationshipOutcome::Stale(r) => Some(r.clone()),
            RelationshipOutcome::Failed(e) | RelationshipOutcome::Blocked(e) => {
                Some(docmonk_telemetry::redact::redact_urls_in_text(e.message()))
            }
            _ => None,
        }
    }

    pub fn is_failure(&self) -> bool {
        matches!(
            self,
            RelationshipOutcome::Failed(_)
                | RelationshipOutcome::Blocked(_)
                | RelationshipOutcome::Stale(_)
        )
    }

    pub fn run(&self) -> Option<&GraphRun> {
        match self {
            RelationshipOutcome::Built(r) | RelationshipOutcome::UpToDate(r) => Some(r),
            _ => None,
        }
    }

    /// JSON summary row.
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "relationship_state": self.state(),
            "relationships": self.run(),
            "relationship_error": self.error(),
        })
    }
}

/// A completed graph stage.
#[derive(Debug, Clone, Serialize)]
pub struct GraphRun {
    pub graph: GraphReport,
    /// Server mode: the graph generation publication.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_publish: Option<docmonk_backends::graph::GraphPublishReport>,
}

fn from_graph(r: Result<GraphRun, GraphError>) -> RelationshipOutcome {
    match r {
        Ok(run)
            if run.graph.outcome == GraphOutcome::UpToDate
                && run.server_publish.as_ref().is_none_or(|p| !p.published) =>
        {
            RelationshipOutcome::UpToDate(Box::new(run))
        }
        Ok(run) => RelationshipOutcome::Built(Box::new(run)),
        Err(GraphError::Stale(r)) => RelationshipOutcome::Stale(r),
        Err(GraphError::NotPublished) => {
            RelationshipOutcome::Skipped("no published index yet".into())
        }
        Err(e) => RelationshipOutcome::Failed(DocMonkError::new(ErrorKind::Generic, e.to_string())),
    }
}

/// Phase 2 for one source. The caller holds the source's lock.
pub fn graph_one(
    home: &Home,
    cfg: &docmonk_config::DocMonkConfig,
    backend: &Backend,
    source: &SourceRecord,
    opts: &Options,
    tracker: Option<&ProgressTracker>,
) -> RelationshipOutcome {
    let mut progress = |done: usize, total: usize| {
        if let Some(t) = tracker {
            t.source_relationships(&source.id, "building", Some((done, total)), None);
        }
    };
    let phase = |name: &'static str| {
        if let Some(t) = tracker {
            t.source_relationships_phase(&source.id, name);
        }
    };
    let facts =
        docmonk_convert::open_graph_facts(cfg, Some(&docmonk_convert::graph_facts_path(home)));
    match backend {
        Backend::Local => {
            let active = match control_plane(home).and_then(|cp| cp.state(&source.id).map_err(db)) {
                Ok(s) => s.active_build_id,
                Err(e) => return RelationshipOutcome::Failed(e),
            };
            let Some(build) = active else {
                return RelationshipOutcome::Skipped("no published index yet".into());
            };
            let mut store = match ProjectStore::open(
                &StorageLayout::new(home),
                &project_id_for_canonical(&source.path),
                &source.id,
                cfg.runtime.sqlite_cache_size_mb,
            ) {
                Ok(s) => s,
                Err(e) => return RelationshipOutcome::Failed(db(e)),
            };
            from_graph(
                docmonk_knowledge::build_graph_phased(
                    &mut store,
                    Path::new(&source.path),
                    &build,
                    opts.workers,
                    None,
                    &mut progress,
                    Some(&phase),
                    facts.as_deref(),
                )
                .map(|graph| GraphRun {
                    graph,
                    server_publish: None,
                }),
            )
        }
        Backend::Server(server) => {
            match crate::server_index::build_relationships(
                home,
                cfg,
                server,
                source,
                opts,
                &mut progress,
                &phase,
            ) {
                Ok(ServerGraphRun { graph, publish }) => from_graph(Ok(GraphRun {
                    graph,
                    server_publish: Some(publish),
                })),
                Err(ServerGraphError::Blocked(e)) => RelationshipOutcome::Blocked(e),
                Err(ServerGraphError::Graph(e)) => from_graph(Err(e)),
                Err(ServerGraphError::Other(e)) => RelationshipOutcome::Failed(e),
            }
        }
    }
}

/// `indexing.relationships_enabled: false`: records the graph as disabled
/// (retained graph rows are hidden from readers; no other indexed
/// knowledge is touched). No graph job runs.
pub fn mark_disabled(
    home: &Home,
    cfg: &docmonk_config::DocMonkConfig,
    backend: &Backend,
    source: &SourceRecord,
) -> Result<(), DocMonkError> {
    match backend {
        Backend::Local => {
            let layout = StorageLayout::new(home);
            let project = project_id_for_canonical(&source.path);
            if !layout.project_db(&project).exists() {
                return Ok(());
            }
            let mut store = ProjectStore::open(
                &layout,
                &project,
                &source.id,
                cfg.runtime.sqlite_cache_size_mb,
            )
            .map_err(db)?;
            docmonk_code::graph_stage::mark_disabled(&mut store).map_err(db)
        }
        Backend::Server(server) => {
            crate::server_index::mark_relationships_disabled(cfg, server, source)
        }
    }
}

/// Phase 2 over `sources`: up to `parallel` sources at once, each under
/// its own lock. `on_outcome` runs on the calling thread.
#[allow(clippy::too_many_arguments)]
pub fn run_stage(
    home: &Home,
    cfg: &docmonk_config::DocMonkConfig,
    backend: &Backend,
    sources: &[SourceRecord],
    operation: &str,
    parallel: usize,
    opts: &Options,
    tracker: Option<&ProgressTracker>,
    mut on_outcome: impl FnMut(&SourceRecord, RelationshipOutcome),
) {
    if sources.is_empty() {
        return;
    }
    docmonk_indexing::executor::run_bounded(
        sources,
        parallel.max(1),
        |source, emit| {
            if let Some(t) = tracker {
                t.source_relationships(&source.id, "building", None, None);
            }
            let outcome = match RunLock::acquire(
                &source_lock_path(home, &source.id),
                operation,
                Some(&source.id),
                opts.lock_timeout,
            ) {
                Err(e) => RelationshipOutcome::Blocked(e),
                Ok(lock) => {
                    let o = graph_one(home, cfg, backend, source, opts, tracker);
                    lock.release();
                    o
                }
            };
            if let Some(t) = tracker {
                t.source_relationships(
                    &source.id,
                    outcome.state(),
                    None,
                    outcome.error().as_deref(),
                );
            }
            emit.emit(outcome);
        },
        |_| {
            RelationshipOutcome::Failed(DocMonkError::new(
                ErrorKind::Generic,
                "relationship build panicked; nothing was published",
            ))
        },
        |i, outcome| {
            let source = &sources[i];
            if let Some(e) = outcome.error() {
                tracing::warn!(component = "relationships", event = "graph_not_published", source_id = %source.id, state = outcome.state(), error = %e);
            }
            on_outcome(source, outcome);
        },
    );
}

/// Totals of one relationship stage.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct StageSummary {
    pub attempted: usize,
    pub built: usize,
    pub up_to_date: usize,
    pub stale: usize,
    pub failed: usize,
    pub skipped: usize,
    /// Published graphs by planned action: current (no work), rebound to
    /// an equivalent snapshot, incrementally updated, fully derived, and
    /// retried after a failure.
    pub graph_skipped: usize,
    pub graph_rebound: usize,
    pub graph_incremental: usize,
    pub graph_full: usize,
    pub graph_retried: usize,
    /// Work done, summed over sources.
    pub files_processed: usize,
    pub files_reresolved: usize,
    pub resolutions_changed: usize,
    pub relationships_written: usize,
    /// Server mode: files copied forward / written, records copied forward
    /// and records adopted from the server into a staging store.
    pub files_copied: usize,
    pub files_written: usize,
    pub records_copied: u64,
    pub records_hydrated: usize,
    pub planning_seconds: f64,
    /// Seconds per graph phase and request/statement counts, summed over
    /// sources (where a long build spends its time).
    pub derivation_seconds: f64,
    pub resolution_seconds: f64,
    pub linking_seconds: f64,
    pub commit_seconds: f64,
    pub server_staging_seconds: f64,
    pub server_copy_seconds: f64,
    pub server_bulk_seconds: f64,
    pub server_promotion_seconds: f64,
    pub sql_statements: u64,
    pub server_http_requests: u64,
    /// Cross-source dependency stage (`relationships build` only; an index
    /// run reports it in its own summary).
    #[serde(skip_serializing_if = "is_default")]
    pub dependencies: crate::dependencies::DependencySummary,
}

fn is_default(d: &crate::dependencies::DependencySummary) -> bool {
    *d == crate::dependencies::DependencySummary::default()
}

impl StageSummary {
    pub fn add(&mut self, o: &RelationshipOutcome) {
        self.attempted += 1;
        match o {
            RelationshipOutcome::Built(_) => self.built += 1,
            RelationshipOutcome::UpToDate(_) => self.up_to_date += 1,
            RelationshipOutcome::Stale(_) => self.stale += 1,
            RelationshipOutcome::Failed(_) | RelationshipOutcome::Blocked(_) => self.failed += 1,
            RelationshipOutcome::Skipped(_) => self.skipped += 1,
        }
        let Some(run) = o.run() else {
            return;
        };
        let g = &run.graph;
        use docmonk_code::graph_stage::PlanAction as A;
        match g.action {
            A::Skip | A::Disabled => self.graph_skipped += 1,
            A::Rebind => self.graph_rebound += 1,
            A::Incremental => self.graph_incremental += 1,
            A::Full => self.graph_full += 1,
            A::Retry => self.graph_retried += 1,
        }
        self.files_processed += g.files_processed;
        self.files_reresolved += g.files_reresolved;
        self.resolutions_changed += g.resolutions_changed;
        self.relationships_written += g.relationships_written;
        self.planning_seconds += g.planning_seconds;
        self.derivation_seconds += g.timings.derivation_seconds;
        self.resolution_seconds += g.timings.resolution_seconds;
        self.linking_seconds += g.timings.linking_seconds;
        self.commit_seconds += g.timings.commit_seconds;
        self.sql_statements += g.counters.sql_statements;
        if let Some(p) = &run.server_publish {
            self.server_staging_seconds += p.timings.staging_seconds;
            self.server_copy_seconds += p.timings.copy_seconds;
            self.server_bulk_seconds += p.timings.bulk_seconds + p.timings.refresh_seconds;
            self.server_promotion_seconds += p.timings.promotion_seconds;
            self.sql_statements += p.counters.staging_sql_statements;
            self.server_http_requests += p.counters.http_requests;
            self.files_copied += p.files_copied;
            self.files_written += p.files_written;
            self.records_copied += p.records_copied;
            self.records_hydrated += p.records_hydrated;
            self.relationships_written += p.edges_written + p.links_written;
        }
    }

    /// Sources whose graph is not current after the stage.
    pub fn not_current(&self) -> usize {
        self.failed + self.stale
    }
}

/// `docmonk relationships build [--source ID]`: phase 2 alone, under
/// progress tracking, without reindexing anything.
pub fn build(
    home: &Home,
    sources: &[SourceRecord],
    mut on_outcome: impl FnMut(&SourceRecord, &RelationshipOutcome),
) -> Result<StageSummary, DocMonkError> {
    let cfg = load(home)?;
    if !cfg.indexing.relationships_enabled {
        return Err(DocMonkError::usage(
            "relationships are disabled (indexing.relationships_enabled: false); enable them to build relationship graphs",
        ));
    }
    let backend = crate::backend::open_for_write(home)?;
    let opts = Options::from_config(&cfg);
    let parallel = cfg.indexing.resolved_max_parallel_sources().max(1);
    let mut summary = StageSummary::default();
    docmonk_indexing::progress::track(
        &home.index_progress(),
        "relationships",
        Some(sources.len() as i64),
        |tracker| {
            tracker.set_run(&crate::indexing::new_run_id(), parallel);
            tracker.set_phase("relationships");
            run_stage(
                home,
                &cfg,
                &backend,
                sources,
                "relationships",
                parallel,
                &opts,
                Some(tracker),
                |source, outcome| {
                    summary.add(&outcome);
                    on_outcome(source, &outcome);
                },
            );
            // Consumers of the rebuilt graphs, with the same planner.
            match crate::sources::catalog(home).and_then(|c| c.list(false)) {
                Ok(all) => crate::dependencies::run_stage(
                    home,
                    &cfg,
                    &backend,
                    &all,
                    opts.lock_timeout,
                    |_, o| summary.dependencies.add(&o),
                ),
                Err(e) => {
                    tracing::warn!(component = "relationships", event = "dependencies_skipped", error = %e.message())
                }
            }
            tracker.set_phase("done");
            Ok::<(), DocMonkError>(())
        },
    )?;
    Ok(summary)
}
