//! The post-index relationship graph stage.
//!
//! Runs only after a source's base index is published, against that exact
//! published build, and never changes a base row:
//!
//! 1. [`plan_graph`] compares the recorded [`GraphState`] with the active
//!    base and yields a [`RelationshipPlan`] with a stable reason code:
//!    `skip` when the graph is `ready` for the active base generation and
//!    derivation; `rebind` when the base was republished with semantically
//!    identical inputs (same [`graph_input_digest`](docmonk_storage::graph::graph_input_digest),
//!    no parsing or resolution); `full` when the graph's rows or dependency
//!    metadata cannot be reused; otherwise `incremental` (or `retry` after a
//!    failure) over the files whose semantic key (kind, status, content
//!    hash; never write times) changed.
//! 2. Each code file to derive is re-read from the source root and its
//!    sha256 compared with the published `content_hash`. A mismatch means
//!    the file changed after indexing: the graph is marked `stale` and
//!    nothing is published (never a graph of mixed snapshots); the next
//!    index pass picks the change up and the graph is retried against it.
//! 3. In one SQLite transaction: verify the base generation is still the
//!    one planned against, replace the changed files' relationships,
//!    re-resolve cross-file references, relink, re-apply manual link intent
//!    and promote the new graph generation. Re-resolution is scoped by the
//!    per-file symbol dependencies ([`FileSymbols`](docmonk_storage::graph::FileSymbols)): the changed files,
//!    plus every unchanged file with a reference whose qualified or bare
//!    name matches a definition the changed or removed files had before or
//!    have now (added, removed, moved or newly ambiguous targets).
//!    Candidates are looked up per symbol, never by loading the build, and
//!    only rows whose outcome changed are rewritten. Any error
//!    rolls everything back and records `failed`; the base index stays
//!    published and searchable throughout.
//!
//! Work and memory are bounded: files are parsed `workers` at a time in
//! batches of [`GRAPH_BATCH`], each batch written before the next is read,
//! and every parse holds one `cpu` permit of the process-wide resource
//! governor, so graph builds of several sources share the indexer's CPU
//! budget instead of each adding its own parser threads.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use docmonk_storage::graph::{FileSymbols, GraphFile, GraphLifecycle, GraphState};
use docmonk_storage::knowledge::{ProjectStore, RelationshipRow};
use docmonk_storage::StorageError;
use serde::Serialize;

use crate::process::{
    file_relationships, graph_derivation, resolve_build, resolve_files, ResolveStats,
};

/// Most files parsed before their relationships are written.
pub const GRAPH_BATCH: usize = 256;

/// What one linking pass loaded, matched and wrote.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct LinkStats {
    /// Automatic links written.
    pub links: usize,
    /// Manual link intents re-applied.
    pub manual: usize,
    pub entities_loaded: usize,
    pub units_loaded: usize,
    pub routes_loaded: usize,
    /// Candidates before deduplication.
    pub candidates: usize,
    /// Only the touched files' entities and routes were loaded (a
    /// code-only change).
    pub narrow: bool,
    /// Loading entities, units, routes and paths.
    pub load_seconds: f64,
    /// Building the inverted unit indexes.
    pub index_seconds: f64,
    /// All matching, and its share per resolver.
    pub match_seconds: f64,
    pub exact_seconds: f64,
    pub qualified_seconds: f64,
    pub alias_seconds: f64,
    pub filename_seconds: f64,
    pub route_seconds: f64,
    pub write_seconds: f64,
    pub manual_seconds: f64,
}

/// Builds code-document links for the graph stage (implemented by the
/// knowledge linker; `None` derives code relationships only).
pub trait GraphLinker: Sync {
    /// Links `touched` files (every file when `full`) and re-applies
    /// manual link intent. Runs inside the graph transaction.
    fn link(
        &self,
        store: &mut ProjectStore,
        build_id: &str,
        touched: &[String],
        full: bool,
    ) -> Result<LinkStats, StorageError>;
}

/// Phase names reported while a graph is built (`deriving`, `resolving`,
/// `linking`, `committing`; server mode adds `staging`, `copying`,
/// `writing` and `promoting`).
pub type GraphPhaseFn<'a> = dyn Fn(&'static str) + 'a;

pub struct GraphOptions<'a> {
    /// Parser threads (at least 1).
    pub workers: usize,
    pub linker: Option<&'a dyn GraphLinker>,
    /// Checked between batches; a cancelled build publishes nothing.
    pub cancel: Option<&'a AtomicBool>,
    /// Called when the build enters a phase.
    pub phase: Option<&'a GraphPhaseFn<'a>>,
    /// Graph facts stored by the base index (`indexing.graph_fact_cache`):
    /// a file whose verified bytes match a stored key is not parsed again.
    pub facts: Option<&'a docmonk_storage::facts::FactCache>,
    /// Files parsed per batch before their rows are written (0:
    /// [`GRAPH_BATCH`]).
    pub batch: usize,
}

/// Wall-clock seconds per phase of one graph build (monotonic timers).
/// `read_hash_seconds` and `parse_seconds` are summed over the parser
/// threads, so with several workers they can exceed `derivation_seconds`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct GraphTimings {
    /// Graph state, dependency manifest and file rows of the build.
    pub state_load_seconds: f64,
    /// Removing the rows that are recomputed.
    pub clear_seconds: f64,
    /// Reading, verifying and parsing files (wall clock).
    pub derivation_seconds: f64,
    pub read_hash_seconds: f64,
    pub parse_seconds: f64,
    /// Fact-cache lookups (summed over the parser threads).
    pub cache_load_seconds: f64,
    /// Writing derived relationship rows.
    pub sqlite_write_seconds: f64,
    /// Reading per-file symbol dependencies.
    pub symbol_manifest_seconds: f64,
    pub resolution_seconds: f64,
    pub linking_seconds: f64,
    /// Recording the graph state and committing the transaction.
    pub commit_seconds: f64,
    /// How long the build held the knowledge database's write lock (from
    /// swapping the staged rows in to the commit; parsing never holds it).
    pub write_lock_seconds: f64,
}

/// Work counted during one graph build.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct GraphCounters {
    /// File rows of the base build.
    pub files_total: usize,
    /// Code files read and hashed (and parsed, unless their facts were
    /// cached).
    pub files_derived: usize,
    /// Of those, files whose graph facts came from the fact cache, and
    /// files parsed because the cache had none (only with a cache).
    pub facts_hits: usize,
    pub facts_misses: usize,
    /// Cache entries of superseded content dropped after the build.
    pub facts_dropped: usize,
    pub bytes_read: u64,
    pub resolution: ResolveStats,
    /// SQL statements the build issued on its own thread.
    pub sql_statements: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GraphOutcome {
    /// The published graph already matches the active base.
    #[default]
    UpToDate,
    /// The committed graph was rebound to an equivalent new base snapshot.
    Rebound,
    Built,
}

/// What one graph build did.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct GraphReport {
    pub outcome: GraphOutcome,
    pub action: PlanAction,
    /// Stable reason code of the plan.
    pub reason: &'static str,
    /// Recomputed from scratch (no usable dependency metadata).
    pub full: bool,
    pub generation: u64,
    pub base_generation: String,
    /// Files whose graph rows were recomputed.
    pub files_processed: usize,
    pub relationships_written: usize,
    /// Cross-file references whose resolution changed.
    pub resolutions_changed: usize,
    /// Files whose cross-file references were re-resolved (every file for
    /// a full build; changed files plus their symbol dependents otherwise).
    pub files_reresolved: usize,
    pub links: usize,
    /// Published changes the plan was made from.
    pub files_added: usize,
    pub files_modified: usize,
    pub files_removed: usize,
    /// Time spent planning (graph state and file rows only).
    pub planning_seconds: f64,
    pub seconds: f64,
    /// Per-phase timings; zero for phases a skip or rebind never ran.
    pub timings: GraphTimings,
    pub counters: GraphCounters,
    pub linking: LinkStats,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GraphError {
    /// No published base build to derive from.
    #[error("no published index to build relationships from")]
    NotPublished,
    /// The inputs no longer match the published base (a file changed on
    /// disk, or the base was republished); retried after the next index.
    #[error("relationships are stale: {0}")]
    Stale(String),
    #[error("relationship build cancelled")]
    Cancelled,
    #[error("relationship build failed: {0}")]
    Failed(String),
}

fn failed(e: impl std::fmt::Display) -> GraphError {
    GraphError::Failed(e.to_string())
}

/// Progress of one graph build: `(files done, files to process)`.
pub type GraphProgressFn<'a> = dyn FnMut(usize, usize) + 'a;

/// What the graph stage does for one source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PlanAction {
    /// The committed graph is current for the active base.
    #[default]
    Skip,
    /// The base was republished with semantically identical inputs: the
    /// committed rows are rebound to the new snapshot without parsing or
    /// resolving anything.
    Rebind,
    /// Only the changed files (and their symbol dependents) are derived.
    Incremental,
    /// Everything is derived from scratch.
    Full,
    /// The last attempt failed or went stale; derived again (incrementally
    /// when the dependency metadata allows it).
    Retry,
    /// `indexing.relationships_enabled: false`.
    Disabled,
}

impl PlanAction {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanAction::Skip => "skip",
            PlanAction::Rebind => "rebind",
            PlanAction::Incremental => "incremental",
            PlanAction::Full => "full",
            PlanAction::Retry => "retry",
            PlanAction::Disabled => "disabled",
        }
    }
}

/// The published changes between the committed graph's inputs and the
/// active base, from the authoritative published file rows (a failed or
/// offline pass publishes nothing, so it never yields a change set).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PublishedChangeSet {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub removed: Vec<String>,
    /// `false` when the previous inputs are unknown: every file is treated
    /// as changed (a conservative full derivation).
    pub complete: bool,
}

impl PublishedChangeSet {
    pub fn is_empty(&self) -> bool {
        self.complete
            && self.added.is_empty()
            && self.modified.is_empty()
            && self.removed.is_empty()
    }
}

/// The graph work planned for one source against one published base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RelationshipPlan {
    pub action: PlanAction,
    /// Stable reason code (`current`, `first_build`, `files_changed`, ...).
    pub reason: &'static str,
    /// Publication fence: the base generation the plan was made against.
    pub expected_base: String,
    pub input_digest: String,
    /// Derive every file (no reusable dependency metadata).
    pub full: bool,
    #[serde(skip)]
    pub changes: PublishedChangeSet,
}

/// Plans the graph work of `build_id`, the source's active published build
/// (cheap: reads only the graph state and the build's file rows).
pub fn plan_graph(store: &ProjectStore, build_id: &str) -> Result<RelationshipPlan, GraphError> {
    let Some(generation) = store.base_generation(build_id).map_err(failed)? else {
        return Err(GraphError::NotPublished);
    };
    let prev = store.graph_state_with_files().map_err(failed)?;
    let files = store.graph_files(build_id).map_err(failed)?;
    let extra = format!("manual:{}", store.manual_link_revision().map_err(failed)?);
    Ok(plan_from(
        &prev,
        build_id,
        generation,
        &files,
        &graph_derivation(),
        &extra,
    ))
}

fn plan_from(
    prev: &GraphState,
    build_id: &str,
    generation: String,
    files: &[GraphFile],
    derivation: &str,
    extra: &str,
) -> RelationshipPlan {
    let digest = docmonk_storage::graph::graph_input_digest(
        derivation,
        files.iter().map(|f| (f.id.as_str(), f.key.as_str())),
        extra,
    );
    let same_rows = prev.base_build_id.as_deref() == Some(build_id);
    let same_derivation = prev.derivation_version.as_deref() == Some(derivation);
    let complete = same_rows && same_derivation && prev.manifest_usable();
    let mut changes = PublishedChangeSet {
        complete,
        ..PublishedChangeSet::default()
    };
    let current: std::collections::BTreeSet<&str> = files.iter().map(|f| f.id.as_str()).collect();
    for f in files {
        match prev.files.get(&f.id) {
            _ if !complete => changes.modified.push(f.id.clone()),
            None => changes.added.push(f.id.clone()),
            Some(k) if *k != f.key => changes.modified.push(f.id.clone()),
            Some(_) => {}
        }
    }
    if complete {
        changes.removed = prev
            .files
            .keys()
            .filter(|f| !current.contains(f.as_str()))
            .cloned()
            .collect();
    }
    let plan = |action, reason, full| RelationshipPlan {
        action,
        reason,
        expected_base: generation.clone(),
        input_digest: digest.clone(),
        full,
        changes: changes.clone(),
    };
    if prev.state == GraphLifecycle::Ready
        && prev.base_generation.as_deref() == Some(generation.as_str())
        && same_derivation
        && prev.input_digest.as_deref() == Some(digest.as_str())
    {
        return plan(PlanAction::Skip, "current", false);
    }
    if complete && prev.input_digest.as_deref() == Some(digest.as_str()) {
        return plan(PlanAction::Rebind, "equivalent_snapshot", false);
    }
    let retry = matches!(
        prev.state,
        GraphLifecycle::Failed | GraphLifecycle::Stale | GraphLifecycle::Building
    );
    if !complete {
        let reason = if prev.base_build_id.is_none() {
            "first_build"
        } else if !same_derivation {
            "derivation_changed"
        } else if !same_rows {
            "base_rebuilt"
        } else {
            prev.manifest_error.unwrap_or("metadata_missing")
        };
        let action = if retry {
            PlanAction::Retry
        } else {
            PlanAction::Full
        };
        return plan(action, reason, true);
    }
    if retry {
        return plan(PlanAction::Retry, "retry_after_failure", false);
    }
    plan(PlanAction::Incremental, "files_changed", false)
}

/// Builds (or confirms) the relationship graph of `build_id`, the source's
/// active published build. The caller holds the source's lock (and lease).
pub fn build_graph(
    store: &mut ProjectStore,
    root: &Path,
    build_id: &str,
    opts: &GraphOptions<'_>,
    progress: &mut GraphProgressFn<'_>,
) -> Result<GraphReport, GraphError> {
    let started = Instant::now();
    let statements = docmonk_storage::stats::sql_statements();
    let plan = plan_graph(store, build_id)?;
    let planning_seconds = started.elapsed().as_secs_f64();
    tracing::debug!(component = "relationships", event = "graph_planned", action = ?plan.action, reason = plan.reason, added = plan.changes.added.len(), modified = plan.changes.modified.len(), removed = plan.changes.removed.len());
    build_planned(store, root, build_id, &plan, opts, progress).map(|mut r| {
        r.planning_seconds = planning_seconds;
        r.seconds = started.elapsed().as_secs_f64();
        r.counters.sql_statements = docmonk_storage::stats::sql_statements() - statements;
        r
    })
}

/// Executes `plan` (from [`plan_graph`]).
pub fn build_planned(
    store: &mut ProjectStore,
    root: &Path,
    build_id: &str,
    plan: &RelationshipPlan,
    opts: &GraphOptions<'_>,
    progress: &mut GraphProgressFn<'_>,
) -> Result<GraphReport, GraphError> {
    let generation = plan.expected_base.clone();
    let derivation = graph_derivation();
    let loaded = Instant::now();
    let prev = store.graph_state_with_files().map_err(failed)?;
    let state_seconds = loaded.elapsed().as_secs_f64();
    let mut report = GraphReport {
        action: plan.action,
        reason: plan.reason,
        generation: prev.generation,
        base_generation: generation.clone(),
        files_added: plan.changes.added.len(),
        files_modified: plan.changes.modified.len(),
        files_removed: plan.changes.removed.len(),
        ..GraphReport::default()
    };
    match plan.action {
        PlanAction::Skip => return Ok(report),
        PlanAction::Rebind => {
            // Same rows, same inputs: only the snapshot binding moves,
            // fenced in one transaction against a concurrent publication.
            store.begin_session().map_err(failed)?;
            let fenced = store.base_generation(build_id).map_err(failed)?;
            if fenced.as_deref() != Some(generation.as_str()) {
                let _ = store.rollback_session();
                return Err(GraphError::Stale(
                    "the base index was republished during the relationship build".into(),
                ));
            }
            let mut state = prev.clone();
            state.state = GraphLifecycle::Ready;
            state.base_generation = Some(generation.clone());
            state.last_error = None;
            state.stale_reason = None;
            // The rows match the inputs exactly, so the symbol scope read
            // from them is exact too.
            state.resolve_all = false;
            store.put_graph_state(&state).map_err(failed)?;
            store.commit_session().map_err(failed)?;
            report.outcome = GraphOutcome::Rebound;
            return Ok(report);
        }
        PlanAction::Disabled => return Ok(report),
        _ => {}
    }
    store
        .set_graph_lifecycle(GraphLifecycle::Building, prev.last_error.as_deref(), None)
        .map_err(failed)?;
    let outcome = build(
        store,
        root,
        build_id,
        &generation,
        &derivation,
        &prev,
        plan,
        opts,
        progress,
    );
    match outcome {
        Ok(mut built) => {
            built.timings.state_load_seconds += state_seconds;
            built.action = plan.action;
            built.reason = plan.reason;
            built.files_added = report.files_added;
            built.files_modified = report.files_modified;
            built.files_removed = report.files_removed;
            Ok(built)
        }
        Err(e) => {
            if store.in_session() {
                let _ = store.rollback_session();
            }
            let (state, error, stale) = match &e {
                GraphError::Stale(reason) => (GraphLifecycle::Stale, None, Some(reason.as_str())),
                GraphError::Cancelled => (
                    GraphLifecycle::Failed,
                    Some("cancelled before completion"),
                    None,
                ),
                GraphError::Failed(m) => (GraphLifecycle::Failed, Some(m.as_str()), None),
                GraphError::NotPublished => (GraphLifecycle::Pending, None, None),
            };
            let _ = store.set_graph_lifecycle(state, error, stale);
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build(
    store: &mut ProjectStore,
    root: &Path,
    build_id: &str,
    generation: &str,
    derivation: &str,
    prev: &GraphState,
    plan: &RelationshipPlan,
    opts: &GraphOptions<'_>,
    progress: &mut GraphProgressFn<'_>,
) -> Result<GraphReport, GraphError> {
    let mut timings = GraphTimings::default();
    let mut counters = GraphCounters::default();
    let phase = |name: &'static str| {
        docmonk_indexing::progress::heartbeat();
        if let Some(f) = opts.phase {
            f(name);
        }
    };
    let mut clock = Instant::now();
    let mut lap = |slot: &mut f64| {
        *slot += clock.elapsed().as_secs_f64();
        clock = Instant::now();
    };
    let files = store.graph_files(build_id).map_err(failed)?;
    counters.files_total = files.len();
    // Incremental only on top of a complete record of this base build:
    // its file keys and its per-file symbol dependencies.
    let full = plan.full;
    let changed_set: std::collections::BTreeSet<&str> = plan
        .changes
        .added
        .iter()
        .chain(&plan.changes.modified)
        .map(String::as_str)
        .collect();
    let changed: Vec<&GraphFile> = files
        .iter()
        .filter(|f| full || changed_set.contains(f.id.as_str()))
        .collect();
    let current: std::collections::BTreeSet<&str> = files.iter().map(|f| f.id.as_str()).collect();
    let removed: Vec<&String> = plan.changes.removed.iter().collect();
    let total = changed.len();
    lap(&mut timings.state_load_seconds);
    phase("deriving");
    progress(0, total);

    // Derive outside the write transaction: files are read, verified and
    // parsed batch by batch, and their rows staged in a connection-local
    // TEMP table, so the knowledge database stays writable for everyone
    // else meanwhile. The write lock is taken only to swap the rows in,
    // resolve, link and commit.
    store.stage_graph_rows_begin().map_err(failed)?;
    let workers = opts.workers.max(1);
    let mut done = 0;
    let mut written = 0;
    let batch_size = if opts.batch == 0 {
        GRAPH_BATCH
    } else {
        opts.batch
    };
    let staged = (|| -> Result<(), GraphError> {
        for batch in changed.chunks(batch_size) {
            if opts.cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                return Err(GraphError::Cancelled);
            }
            docmonk_indexing::progress::heartbeat();
            let code: Vec<&GraphFile> = batch
                .iter()
                .copied()
                .filter(|f| f.kind == "code" && f.status == "indexed")
                .collect();
            let derived = derive_batch(root, &code, workers, opts.facts)?;
            lap(&mut timings.derivation_seconds);
            for d in derived {
                counters.files_derived += 1;
                counters.bytes_read += d.bytes;
                timings.read_hash_seconds += d.read_hash_seconds;
                timings.parse_seconds += d.parse_seconds;
                timings.cache_load_seconds += d.cache_load_seconds;
                match d.cached {
                    Some(true) => counters.facts_hits += 1,
                    Some(false) => counters.facts_misses += 1,
                    None => {}
                }
                written += d.rows.len();
                store.stage_graph_rows(&d.rows).map_err(failed)?;
            }
            lap(&mut timings.sqlite_write_seconds);
            done += batch.len();
            progress(done, total);
        }
        Ok(())
    })();
    if let Err(e) = staged {
        let _ = store.stage_graph_rows_end();
        return Err(e);
    }

    let locked = Instant::now();
    store.begin_session().map_err(failed)?;
    // Fenced against a concurrent publication: the base generation must
    // still be the one this build was planned against.
    if store.base_generation(build_id).map_err(failed)?.as_deref() != Some(generation) {
        return Err(GraphError::Stale(
            "the base index was republished during the relationship build".into(),
        ));
    }
    if full {
        store.clear_graph(build_id).map_err(failed)?;
    } else {
        for f in &changed {
            store.clear_file_graph(build_id, &f.id).map_err(failed)?;
        }
        // Rows of removed files and links to vanished entities or
        // documents (the base update left them for readers).
        store.clear_orphaned_graph(build_id).map_err(failed)?;
    }
    lap(&mut timings.clear_seconds);
    store.publish_staged_graph_rows(build_id).map_err(failed)?;
    store.stage_graph_rows_end().map_err(failed)?;
    lap(&mut timings.sqlite_write_seconds);
    phase("resolving");
    let changed_ids: Vec<String> = changed.iter().map(|f| f.id.clone()).collect();
    let (symbols, resolution, files_reresolved) = if full || prev.resolve_all {
        let n = resolve_build(store, build_id).map_err(failed)?;
        lap(&mut timings.resolution_seconds);
        let symbols = store.file_symbols(build_id, None).map_err(failed)?;
        lap(&mut timings.symbol_manifest_seconds);
        (symbols, n, files.len())
    } else {
        // Symbols whose candidates changed: a definition added, removed,
        // renamed, re-identified or reordered in a changed or removed file
        // (a body-only edit changes none).
        let fresh = store
            .file_symbols(build_id, Some(&changed_ids))
            .map_err(failed)?;
        // The recorded symbols of only the changed and removed files.
        let looked_up: Vec<String> = changed_ids
            .iter()
            .chain(removed.iter().copied())
            .cloned()
            .collect();
        let before = store.graph_symbols(&looked_up).map_err(failed)?;
        lap(&mut timings.symbol_manifest_seconds);
        let mut keys: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for f in &looked_up {
            keys.extend(FileSymbols::changed_keys(before.get(f), fresh.get(f)));
        }
        // Re-resolve the changed files and every unchanged file with a
        // reference through one of those symbols (an indexed lookup).
        let mut targets: std::collections::BTreeSet<String> = changed_ids.iter().cloned().collect();
        let keys: Vec<&str> = keys.into_iter().collect();
        for f in store.graph_dependents(&keys).map_err(failed)? {
            if current.contains(f.as_str()) {
                targets.insert(f);
            }
        }
        lap(&mut timings.symbol_manifest_seconds);
        let n = resolve_files(store, build_id, &targets).map_err(failed)?;
        lap(&mut timings.resolution_seconds);
        // Only the changed files' symbols are written back.
        (fresh, n, targets.len())
    };
    phase("linking");
    let linking = match opts.linker {
        Some(l) => l
            .link(store, build_id, &changed_ids, full)
            .map_err(failed)?,
        None => LinkStats::default(),
    };
    lap(&mut timings.linking_seconds);
    phase("committing");
    let whole_manifest = full || prev.resolve_all;
    let state = GraphState {
        state: GraphLifecycle::Ready,
        generation: prev.generation + 1,
        base_build_id: Some(build_id.to_owned()),
        base_generation: Some(generation.to_owned()),
        derivation_version: Some(derivation.to_owned()),
        input_digest: Some(plan.input_digest.clone()),
        manifest_version: docmonk_storage::graph::MANIFEST_VERSION,
        metadata_complete: true,
        resolve_all: false,
        manifest_error: None,
        files: files
            .iter()
            .map(|f| (f.id.clone(), f.key.clone()))
            .collect(),
        symbols,
        last_success_at: Some(docmonk_storage::db::now_iso()),
        last_error: None,
        stale_reason: None,
        updated_at: None,
    };
    if whole_manifest {
        store.put_graph_state_with_files(&state).map_err(failed)?;
    } else {
        // Proportional to the change: the changed files' keys and symbols,
        // and the removed files.
        let keys: std::collections::BTreeMap<String, String> = files
            .iter()
            .filter(|f| changed_set.contains(f.id.as_str()))
            .map(|f| (f.id.clone(), f.key.clone()))
            .collect();
        let removed: Vec<String> = removed.iter().map(|f| (*f).clone()).collect();
        store
            .put_graph_state_delta(&state, &keys, &state.symbols, &removed)
            .map_err(failed)?;
    }
    store.commit_session().map_err(failed)?;
    lap(&mut timings.commit_seconds);
    timings.write_lock_seconds = locked.elapsed().as_secs_f64();
    if let Some(cache) = opts.facts {
        // Only the current content's facts can be used again.
        let keep: std::collections::HashSet<String> = files
            .iter()
            .filter(|f| f.kind == "code")
            .filter_map(|f| {
                f.content_hash
                    .as_deref()
                    .map(|h| crate::process::graph_fact_key(&f.id, &f.rel_path, h))
            })
            .collect();
        match cache.retain(store.source_id(), &keep) {
            Ok(n) => counters.facts_dropped = n,
            Err(e) => {
                tracing::debug!(component = "relationships", event = "graph_facts_not_collected", error = %e)
            }
        }
    }
    counters.resolution = resolution;
    Ok(GraphReport {
        outcome: GraphOutcome::Built,
        action: plan.action,
        reason: plan.reason,
        full,
        generation: state.generation,
        base_generation: generation.to_owned(),
        files_processed: total,
        relationships_written: written,
        resolutions_changed: resolution.changed,
        files_reresolved,
        links: linking.links,
        timings,
        counters,
        linking,
        ..GraphReport::default()
    })
}

/// One file's derived rows and what deriving them cost.
struct Derived {
    rows: Vec<RelationshipRow>,
    bytes: u64,
    read_hash_seconds: f64,
    parse_seconds: f64,
    cache_load_seconds: f64,
    /// `Some(hit)` when a fact cache was consulted.
    cached: Option<bool>,
}

/// Reads, verifies and parses `files` on up to `workers` threads, in input
/// order.
fn derive_batch(
    root: &Path,
    files: &[&GraphFile],
    workers: usize,
    facts: Option<&docmonk_storage::facts::FactCache>,
) -> Result<Vec<Derived>, GraphError> {
    let per = files.len().div_ceil(workers.max(1)).max(1);
    let parts: Vec<Result<Vec<Derived>, GraphError>> = std::thread::scope(|s| {
        let handles: Vec<_> = files
            .chunks(per)
            .map(|part| s.spawn(move || part.iter().map(|f| derive_file(root, f, facts)).collect()))
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err(failed("relationship derivation panicked")))
            })
            .collect()
    });
    let mut out = Vec::with_capacity(files.len());
    for p in parts {
        out.extend(p?);
    }
    Ok(out)
}

fn derive_file(
    root: &Path,
    f: &GraphFile,
    facts: Option<&docmonk_storage::facts::FactCache>,
) -> Result<Derived, GraphError> {
    let started = Instant::now();
    let source = std::fs::read(root.join(&f.rel_path)).map_err(|e| {
        GraphError::Stale(format!(
            "{} cannot be read since it was indexed ({e})",
            f.rel_path
        ))
    })?;
    let hash = docmonk_indexing::fingerprint::hash_bytes(&source);
    if f.content_hash.as_deref() != Some(hash.as_str()) {
        return Err(GraphError::Stale(format!(
            "{} changed since it was indexed",
            f.rel_path
        )));
    }
    let read_hash_seconds = started.elapsed().as_secs_f64();
    // The bytes are verified against the published hash above, so facts
    // stored for that hash are exactly what parsing them would produce.
    let looking = Instant::now();
    let cached = facts.map(|c| c.get(&crate::process::graph_fact_key(&f.id, &f.rel_path, &hash)));
    let cache_load_seconds = looking.elapsed().as_secs_f64();
    let consulted = cached.is_some();
    if let Some(Some(rows)) = cached {
        return Ok(Derived {
            rows,
            bytes: source.len() as u64,
            read_hash_seconds,
            parse_seconds: 0.0,
            cache_load_seconds,
            cached: Some(true),
        });
    }
    let _cpu =
        docmonk_indexing::governor::global().acquire(docmonk_indexing::governor::Class::Cpu, 1);
    let parsing = Instant::now();
    // The base index parsed these exact bytes; a parse failure here would
    // be a derivation bug, recorded as a failure rather than hidden.
    let rows = file_relationships(&f.rel_path, &f.id, &source).map_err(|p| {
        failed(format!(
            "{}: syntax error(s) in a {} file",
            f.rel_path, p.language
        ))
    })?;
    Ok(Derived {
        rows,
        bytes: source.len() as u64,
        read_hash_seconds,
        parse_seconds: parsing.elapsed().as_secs_f64(),
        cache_load_seconds,
        cached: consulted.then_some(false),
    })
}

/// Marks the graph disabled (`indexing.relationships_enabled: false`):
/// retained graph rows are hidden, nothing else is touched.
pub fn mark_disabled(store: &mut ProjectStore) -> Result<(), StorageError> {
    let s = store.graph_state()?;
    if s.state != GraphLifecycle::Disabled {
        store.set_graph_lifecycle(GraphLifecycle::Disabled, None, None)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use docmonk_storage::graph::{file_key, graph_input_digest};

    fn file(id: &str, hash: &str) -> GraphFile {
        GraphFile {
            id: id.into(),
            rel_path: format!("{id}.py"),
            kind: "code".into(),
            status: "indexed".into(),
            content_hash: Some(hash.into()),
            key: file_key("code", "indexed", Some(hash)),
        }
    }

    fn committed(files: &[GraphFile], generation: &str) -> GraphState {
        GraphState {
            state: GraphLifecycle::Ready,
            generation: 1,
            base_build_id: Some("b".into()),
            base_generation: Some(generation.into()),
            derivation_version: Some(graph_derivation()),
            input_digest: Some(graph_input_digest(
                &graph_derivation(),
                files.iter().map(|f| (f.id.as_str(), f.key.as_str())),
                "",
            )),
            files: files
                .iter()
                .map(|f| (f.id.clone(), f.key.clone()))
                .collect(),
            symbols: [("a".to_owned(), FileSymbols::default())].into(),
            manifest_version: docmonk_storage::graph::MANIFEST_VERSION,
            metadata_complete: true,
            ..GraphState::default()
        }
    }

    fn plan(prev: &GraphState, generation: &str, files: &[GraphFile]) -> RelationshipPlan {
        plan_from(prev, "b", generation.into(), files, &graph_derivation(), "")
    }

    #[test]
    fn a_current_graph_is_skipped() {
        let files = [file("a", "1"), file("b", "2")];
        let p = plan(&committed(&files, "b@t1"), "b@t1", &files);
        assert_eq!((p.action, p.reason), (PlanAction::Skip, "current"));
    }

    #[test]
    fn a_republication_with_identical_content_is_rebound_not_rebuilt() {
        let files = [file("a", "1"), file("b", "2")];
        let p = plan(&committed(&files, "b@t1"), "b@t2", &files);
        assert_eq!(p.action, PlanAction::Rebind);
        assert!(p.changes.is_empty());
    }

    #[test]
    fn only_semantically_changed_files_are_planned() {
        let files = [file("a", "1"), file("b", "2")];
        let prev = committed(&files, "b@t1");
        let now = [file("a", "1"), file("b", "3"), file("c", "4")];
        let p = plan(&prev, "b@t2", &now);
        assert_eq!(p.action, PlanAction::Incremental);
        assert_eq!(p.changes.modified, vec!["b".to_owned()]);
        assert_eq!(p.changes.added, vec!["c".to_owned()]);
        let p = plan(&prev, "b@t2", &files[..1]);
        assert_eq!(p.changes.removed, vec!["b".to_owned()]);
    }

    #[test]
    fn missing_or_failed_graphs_are_scheduled_even_without_index_changes() {
        let files = [file("a", "1")];
        let p = plan(&GraphState::default(), "b@t1", &files);
        assert_eq!(
            (p.action, p.reason, p.full),
            (PlanAction::Full, "first_build", true)
        );
        let mut failed = committed(&files, "b@t0");
        failed.state = GraphLifecycle::Failed;
        failed.input_digest = None;
        let p = plan(&failed, "b@t1", &files);
        assert_eq!(p.action, PlanAction::Retry);
        let mut old = committed(&files, "b@t1");
        old.derivation_version = Some("other".into());
        let p = plan(&old, "b@t1", &files);
        assert_eq!(
            (p.action, p.reason),
            (PlanAction::Full, "derivation_changed")
        );
    }
}
