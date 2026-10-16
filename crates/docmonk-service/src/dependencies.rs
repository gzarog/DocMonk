//! Cross-source relationship dependencies (stage 3 of a run).
//!
//! Source graphs are derived per source ([`crate::relationships`]); the
//! cross-file resolver never looks outside its own source. This stage adds
//! an explicit, scoped layer on top: a **consumer** source's references
//! that stayed `unresolved` locally are looked up, by exact qualified name
//! only, in the **producer** sources it depends on, and the outcome is
//! recorded as the consumer's [`ExternalManifest`] (the source-local graph
//! rows are never changed).
//!
//! * Scope: dependencies are declared (`indexing.relationship_dependencies`,
//!   `"<consumer> -> <producer>"`) or discovered from C# `<ProjectReference>`
//!   items whose target lies inside another registered source. Package
//!   references and project references outside every registered source are
//!   recorded as unsupported, never guessed. A source that is in no
//!   dependency is never consulted, so identical symbols in unrelated
//!   repositories never collide; a name with candidates in more than one
//!   producer is recorded as ambiguous, never bound.
//! * Invalidation: the manifest stores, per producer and looked-up key, the
//!   candidate fingerprint (entity ids, empty for none). A consumer is
//!   updated only when its own graph changed or a fingerprint it depends on
//!   changed (a definition added, removed, renamed or re-identified); a
//!   producer's implementation-only edit leaves every consumer untouched.
//!   Bindings depend on producers' published definitions, never on other
//!   bindings, so the work is one pass over direct consumers: each consumer
//!   is visited at most once per run, and dependency cycles terminate.
//! * Removal: a source whose stored manifest still records producers or
//!   bindings is reconciled even when it no longer depends on anything, so
//!   removed dependencies drop their bindings; a consumer whose discovery
//!   could not complete keeps its last bindings, marked `stale`.
//! * Lookup failures are never read as "no candidate": the consumer keeps
//!   its last bindings, marked `stale`.
//! * Fencing: before the manifest is promoted, the authoritative active
//!   build and base generation of every producer, and the consumer's
//!   active build and graph generation, are re-read; any change meanwhile
//!   leaves the consumer `stale`. A consumer is marked `stale` before it is updated, so
//!   an interrupted run is picked up by the next one; unavailable producers
//!   or consumers are reported `stale`/`pending`, never silently current.
//! * Runs indexing only some sources still update their impacted consumers
//!   against those consumers' existing published bases.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use docmonk_config::model::parse_dependency;
use docmonk_core::errors::DocMonkError;
use docmonk_core::paths::{project_id_for_canonical, Home};
use docmonk_indexing::lock::RunLock;
use docmonk_storage::control::SourceRecord;
use docmonk_storage::graph::{
    candidates_fingerprint, qualified_key, ExternalBinding, ExternalManifest, ProducerSnapshot,
};
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::StorageLayout;
use serde::Serialize;

use crate::backend::{server_err, Backend};
use crate::indexing::{lease_owner, source_lock_path};
use crate::sources::control_plane;
use crate::{db, load};

/// One consumer -> producer dependency.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Dependency {
    pub consumer: String,
    pub producer: String,
    /// `declared` or `project_reference`.
    pub via: &'static str,
}

/// Every known dependency between registered sources.
#[derive(Debug, Default, Clone, Serialize)]
pub struct Registry {
    pub edges: BTreeSet<Dependency>,
    /// `consumer -> references that cannot be followed automatically`.
    pub unsupported: BTreeMap<String, Vec<String>>,
    /// Declarations naming no registered source.
    pub invalid: Vec<String>,
    /// Sources whose stored manifest records producers or bindings: they
    /// are reconciled even when no edge names them any more.
    pub manifest_owners: BTreeSet<String>,
    /// `source -> why` its dependencies could not be fully discovered: a
    /// missing edge there is unknown, never a removal.
    pub incomplete: BTreeMap<String, String>,
}

impl Registry {
    pub fn producers_of(&self, consumer: &str) -> Vec<&Dependency> {
        self.edges
            .iter()
            .filter(|d| d.consumer == consumer)
            .collect()
    }

    pub fn consumers(&self) -> BTreeSet<&str> {
        self.edges.iter().map(|d| d.consumer.as_str()).collect()
    }

    /// Every source to reconcile: current consumers and previous manifest
    /// owners (whose dependencies may have been removed).
    pub fn refresh_candidates(&self) -> BTreeSet<&str> {
        let mut out = self.consumers();
        out.extend(self.manifest_owners.iter().map(String::as_str));
        out
    }
}

/// A source's published build, opened for reading.
pub(crate) struct View {
    pub store: ProjectStore,
    pub build: String,
    pub generation: String,
    /// The authoritative active build id when the view was opened (the
    /// control plane locally, the server's state in server mode).
    pub active: String,
}

/// The authoritative active build id of `source` right now.
fn active_build(
    home: &Home,
    backend: &Backend,
    source: &SourceRecord,
) -> Result<Option<String>, DocMonkError> {
    Ok(match backend {
        Backend::Local => {
            control_plane(home)?
                .state(&source.id)
                .map_err(db)?
                .active_build_id
        }
        Backend::Server(server) => server
            .source_state(&source.id)
            .map_err(server_err)?
            .and_then(|s| s.active_build_id),
    })
}

pub(crate) fn open_view(
    home: &Home,
    cfg: &docmonk_config::DocMonkConfig,
    backend: &Backend,
    source: &SourceRecord,
) -> Result<Option<View>, DocMonkError> {
    let (store, build, active) = match backend {
        Backend::Local => {
            let Some(build) = control_plane(home)?
                .state(&source.id)
                .map_err(db)?
                .active_build_id
            else {
                return Ok(None);
            };
            let layout = StorageLayout::new(home);
            let project = project_id_for_canonical(&source.path);
            if !layout.project_db(&project).exists() {
                return Ok(None);
            }
            let store = ProjectStore::open(
                &layout,
                &project,
                &source.id,
                cfg.runtime.sqlite_cache_size_mb,
            )
            .map_err(db)?;
            (store, build.clone(), build)
        }
        Backend::Server(server) => {
            match crate::server_index::synced_staging_store(home, cfg, server, source)? {
                Some(v) => v,
                None => return Ok(None),
            }
        }
    };
    let Some(generation) = store.base_generation(&build).map_err(db)? else {
        return Ok(None);
    };
    Ok(Some(View {
        store,
        build,
        generation,
        active,
    }))
}

/// Where a producer's published definitions are read from.
enum Definitions {
    /// The local store (local mode).
    Store(ProjectStore),
    /// The authoritative server index at the pinned active build (server
    /// mode): no staging copy of the producer is needed on this host.
    Server(docmonk_backends::reader::ServerReader),
}

/// A producer pinned at its active build for one update.
pub(crate) struct Producer {
    defs: Definitions,
    build: String,
    pub generation: String,
    pub active: String,
}

type Candidates = std::collections::HashMap<String, Vec<docmonk_storage::knowledge::EntityRow>>;

impl Producer {
    /// Exact qualified-name candidates of `names`, batched. An error is a
    /// failed read, never "no candidate".
    fn lookup(&self, names: &[&str]) -> Result<Candidates, String> {
        match &self.defs {
            Definitions::Store(s) => s
                .entities_by_qualified_names(&self.build, names)
                .map_err(|e| e.to_string()),
            Definitions::Server(r) => r
                .entities_by_qualified_names(&self.build, names)
                .map_err(|e| e.to_string()),
        }
    }

    /// The pinned build's base generation now (`None`: no longer published).
    fn generation_now(&self) -> Result<Option<String>, DocMonkError> {
        match &self.defs {
            Definitions::Store(s) => s.base_generation(&self.build).map_err(db),
            // Server builds are immutable; republication changes the active
            // build id, which the fence compares.
            Definitions::Server(_) => Ok(Some(self.generation.clone())),
        }
    }
}

fn open_producer(
    home: &Home,
    cfg: &docmonk_config::DocMonkConfig,
    backend: &Backend,
    source: &SourceRecord,
) -> Result<Option<Producer>, DocMonkError> {
    match backend {
        Backend::Local => Ok(open_view(home, cfg, backend, source)?.map(|v| Producer {
            defs: Definitions::Store(v.store),
            build: v.build,
            generation: v.generation,
            active: v.active,
        })),
        Backend::Server(server) => {
            let Some(active) = active_build(home, backend, source)? else {
                return Ok(None);
            };
            Ok(Some(Producer {
                defs: Definitions::Server(docmonk_backends::reader::ServerReader::new(
                    server.clone(),
                    &source.id,
                )),
                build: active.clone(),
                generation: format!("server:{active}"),
                active,
            }))
        }
    }
}

fn find<'a>(sources: &'a [SourceRecord], r: &str) -> Option<&'a SourceRecord> {
    let resolved = docmonk_core::paths::resolve(Path::new(r)).ok();
    sources.iter().find(|s| {
        s.id == r || s.path == r || resolved.as_deref().is_some_and(|p| Path::new(&s.path) == p)
    })
}

/// Lexically normalizes `p` (`..` and `.`), without touching the disk.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// References of one `.csproj` document. MSBuild is never evaluated:
/// anything whose meaning depends on evaluation (conditions, properties,
/// wildcards, imports) is reported in `unsupported`, never guessed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CsprojReferences {
    pub projects: Vec<String>,
    pub packages: Vec<String>,
    pub unsupported: Vec<String>,
}

/// Parses a `.csproj` document as XML (either quote style, namespaces,
/// comments and child elements are handled by the parser).
pub fn csproj_references(text: &str) -> CsprojReferences {
    let mut out = CsprojReferences::default();
    let doc = match roxmltree::Document::parse(text) {
        Ok(d) => d,
        Err(e) => {
            out.unsupported.push(format!("not well-formed XML: {e}"));
            return out;
        }
    };
    for node in doc.descendants().filter(|n| n.is_element()) {
        let tag = node.tag_name().name();
        match tag {
            "Import" => {
                if let Some(p) = node.attribute("Project") {
                    out.unsupported.push(format!("import {p} is not evaluated"));
                }
                continue;
            }
            "ProjectReference" | "PackageReference" => {}
            _ => continue,
        }
        let Some(include) = node.attribute("Include") else {
            continue;
        };
        let conditional = node
            .ancestors()
            .filter(|a| a.is_element())
            .any(|a| a.attribute("Condition").is_some());
        for item in include.split(';').map(str::trim).filter(|i| !i.is_empty()) {
            let why = if conditional {
                Some("is conditional")
            } else if item.contains("$(") || item.contains("@(") || item.contains("%(") {
                Some("uses an unevaluated property")
            } else if item.contains('*') || item.contains('?') {
                Some("uses a wildcard")
            } else {
                None
            };
            let kind = if tag == "ProjectReference" {
                "project"
            } else {
                "package"
            };
            match why {
                Some(why) => out
                    .unsupported
                    .push(format!("{kind} reference {item} {why}")),
                None if tag == "ProjectReference" => out.projects.push(item.to_owned()),
                None => out.packages.push(item.to_owned()),
            }
        }
    }
    out
}

/// The registered source whose root most specifically contains `target`
/// (nested source roots resolve to the innermost).
fn containing_source<'a>(sources: &'a [SourceRecord], target: &Path) -> Option<&'a SourceRecord> {
    sources
        .iter()
        .filter(|s| target.starts_with(Path::new(&s.path)))
        .max_by_key(|s| Path::new(&s.path).components().count())
}

/// Collects the declared and discovered dependencies of `sources`.
pub fn discover(
    home: &Home,
    cfg: &docmonk_config::DocMonkConfig,
    backend: &Backend,
    sources: &[SourceRecord],
) -> Registry {
    let mut reg = Registry::default();
    for d in &cfg.indexing.relationship_dependencies {
        let Some((c, p)) = parse_dependency(d) else {
            reg.invalid.push(d.clone());
            continue;
        };
        match (find(sources, c), find(sources, p)) {
            (Some(c), Some(p)) if c.id != p.id => {
                reg.edges.insert(Dependency {
                    consumer: c.id.clone(),
                    producer: p.id.clone(),
                    via: "declared",
                });
            }
            _ => reg.invalid.push(d.clone()),
        }
    }
    if let Backend::Server(server) = backend {
        // The authoritative manifests, in one scan (a lost staging cache
        // must not hide a consumer whose dependencies were removed).
        match server.graph_external_owners() {
            Ok(owners) => reg.manifest_owners.extend(owners),
            Err(e) => {
                for s in sources {
                    reg.incomplete
                        .insert(s.id.clone(), format!("manifest owners unreadable: {e}"));
                }
            }
        }
    }
    for source in sources {
        let mut incomplete = |why: String| {
            reg.incomplete.entry(source.id.clone()).or_insert(why);
        };
        let view = match open_view(home, cfg, backend, source) {
            Ok(Some(v)) => v,
            Ok(None) => continue,
            Err(e) => {
                incomplete(format!("published index unreadable: {}", e.message()));
                continue;
            }
        };
        match view.store.external_manifest() {
            Ok(Some(m)) if !m.producers.is_empty() || !m.bindings.is_empty() => {
                reg.manifest_owners.insert(source.id.clone());
            }
            Ok(_) => {}
            Err(e) => incomplete(format!("manifest unreadable: {e}")),
        }
        let projects = match view.store.file_paths_with_suffix(&view.build, ".csproj") {
            Ok(p) => p,
            Err(e) => {
                incomplete(format!("project files unreadable: {e}"));
                continue;
            }
        };
        for rel_path in &projects {
            let path = Path::new(&source.path).join(rel_path);
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    incomplete(format!("{rel_path} unreadable: {e}"));
                    continue;
                }
            };
            let refs = csproj_references(&text);
            let dir = path.parent().unwrap_or(Path::new(&source.path));
            for u in refs.unsupported {
                reg.unsupported
                    .entry(source.id.clone())
                    .or_default()
                    .push(format!("{rel_path}: {u}"));
            }
            let packages = refs.packages;
            for r in refs.projects {
                let target = normalize(&dir.join(r.replace('\\', "/")));
                match containing_source(sources, &target) {
                    Some(p) if p.id == source.id => {}
                    Some(p) => {
                        reg.edges.insert(Dependency {
                            consumer: source.id.clone(),
                            producer: p.id.clone(),
                            via: "project_reference",
                        });
                    }
                    None => reg
                        .unsupported
                        .entry(source.id.clone())
                        .or_default()
                        .push(format!(
                            "{rel_path}: project reference {r} is outside every registered source"
                        )),
                }
            }
            for r in packages {
                reg.unsupported.entry(source.id.clone()).or_default().push(format!(
                    "{rel_path}: package reference {r} (declare it in indexing.relationship_dependencies)"
                ));
            }
        }
    }
    reg
}

/// What the dependency stage did for one consumer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DependencyOutcome {
    /// Nothing the consumer depends on changed.
    Current,
    /// Bindings were recomputed.
    Updated {
        /// Recomputed from scratch (consumer graph changed, or no usable
        /// manifest).
        full: bool,
        /// Producer keys whose candidates changed.
        keys_changed: usize,
        /// References looked up again.
        references: usize,
        /// References bound to exactly one producer candidate.
        bound: usize,
        /// The producers that triggered the update.
        triggered_by: Vec<String>,
    },
    /// Not current; retried by the next run.
    Stale { reason: String },
    /// Cannot be computed now (consumer disabled or not published yet).
    Pending { reason: String },
}

impl DependencyOutcome {
    pub fn state(&self) -> &'static str {
        match self {
            DependencyOutcome::Current => "current",
            DependencyOutcome::Updated { .. } => "updated",
            DependencyOutcome::Stale { .. } => "stale",
            DependencyOutcome::Pending { .. } => "pending",
        }
    }
}

/// Totals of one dependency stage.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct DependencySummary {
    pub consumers: usize,
    pub current: usize,
    pub updated: usize,
    pub stale: usize,
    pub pending: usize,
}

impl DependencySummary {
    pub fn add(&mut self, o: &DependencyOutcome) {
        self.consumers += 1;
        match o {
            DependencyOutcome::Current => self.current += 1,
            DependencyOutcome::Updated { .. } => self.updated += 1,
            DependencyOutcome::Stale { .. } => self.stale += 1,
            DependencyOutcome::Pending { .. } => self.pending += 1,
        }
    }
}

/// Why a producer lookup failed (the storage error, never "no candidate").
fn lookup_failed(producer: &str, e: &impl std::fmt::Display) -> String {
    format!("lookup in producer {producer} failed (retryable): {e}")
}

fn stale(reason: impl Into<String>) -> DependencyOutcome {
    DependencyOutcome::Stale {
        reason: reason.into(),
    }
}

/// Records `m` as the next revision: in server mode on the authoritative
/// copy first, then on the consumer's staging cache (derived; repaired from
/// the server whenever it lags).
fn persist(
    cfg: &docmonk_config::DocMonkConfig,
    backend: &Backend,
    consumer: &SourceRecord,
    store: &mut ProjectStore,
    m: &mut ExternalManifest,
) -> Result<(), DocMonkError> {
    m.revision += 1;
    m.updated_at = Some(docmonk_storage::db::now_iso());
    if let Backend::Server(server) = backend {
        let ttl = std::time::Duration::from_secs_f64(cfg.storage.server.lease_seconds);
        let lease = server
            .acquire_lease(&consumer.id, &lease_owner(), ttl)
            .map_err(server_err)?;
        let value = serde_json::to_value(&*m).map_err(crate::generic)?;
        let r = server
            .set_graph_external(&consumer.id, Some(&lease), &value)
            .map_err(server_err);
        let _ = server.release_lease(&lease);
        r?;
    }
    store.put_external_manifest(m).map_err(db)?;
    Ok(())
}

/// The consumer's manifest: in server mode the authoritative copy, unless
/// the staging cache is at least as recent (a lagging or lost cache is
/// repaired from it).
fn load_manifest(
    backend: &Backend,
    consumer: &SourceRecord,
    store: &mut ProjectStore,
) -> Result<ExternalManifest, DocMonkError> {
    let cached = store.external_manifest().map_err(db)?;
    let Backend::Server(server) = backend else {
        return Ok(cached.unwrap_or_default());
    };
    let authoritative: Option<ExternalManifest> = server
        .graph_external(&consumer.id)
        .map_err(server_err)?
        .map(serde_json::from_value)
        .transpose()
        .map_err(crate::generic)?;
    Ok(match (cached, authoritative) {
        (Some(c), Some(a)) if c.revision >= a.revision && a.revision > 0 => c,
        (_, Some(a)) => {
            store.put_external_manifest(&a).map_err(db)?;
            a
        }
        (c, None) => c.unwrap_or_default(),
    })
}

/// Test hooks for deterministic race reproductions. Not part of the API.
#[doc(hidden)]
pub mod test_hooks {
    use std::sync::Mutex;

    type Hook = Box<dyn Fn(&str) + Send>;
    static BEFORE_FENCE: Mutex<Option<Hook>> = Mutex::new(None);

    /// Runs `hook(consumer_id)` after bindings are computed and before the
    /// publication fence (`None` clears it).
    pub fn set_before_fence(hook: Option<Hook>) {
        *BEFORE_FENCE.lock().unwrap_or_else(|e| e.into_inner()) = hook;
    }

    pub(crate) fn before_fence(consumer: &str) {
        if let Some(h) = &*BEFORE_FENCE.lock().unwrap_or_else(|e| e.into_inner()) {
            h(consumer);
        }
    }
}

/// Updates one consumer's bindings. The caller holds its lock.
fn update_consumer(
    home: &Home,
    cfg: &docmonk_config::DocMonkConfig,
    backend: &Backend,
    reg: &Registry,
    sources: &[SourceRecord],
    consumer: &SourceRecord,
) -> Result<DependencyOutcome, DocMonkError> {
    if !consumer.enabled {
        return Ok(DependencyOutcome::Pending {
            reason: "consumer source is disabled".into(),
        });
    }
    let Some(mut view) = open_view(home, cfg, backend, consumer)? else {
        return Ok(DependencyOutcome::Pending {
            reason: "consumer has no published index on this host".into(),
        });
    };
    let mut manifest = load_manifest(backend, consumer, &mut view.store)?;
    manifest.unsupported = reg
        .unsupported
        .get(&consumer.id)
        .cloned()
        .unwrap_or_default();
    // Bindings are computed against the consumer's current graph only
    // (never the stale one readers may be served meanwhile).
    let freshness = view.store.graph_freshness(&view.build).map_err(db)?;
    if !freshness.visible || freshness.stale {
        if manifest.state != "stale" {
            manifest.state = "stale".into();
            manifest.reason = Some("the consumer's own graph is not current".into());
            persist(cfg, backend, consumer, &mut view.store, &mut manifest)?;
        }
        return Ok(stale("the consumer's own graph is not current"));
    }
    let graph_generation = view.store.graph_state().map_err(db)?.generation;

    // Producers in scope, each opened at its current published base.
    let mut producers: BTreeMap<String, (Producer, &'static str)> = BTreeMap::new();
    for d in reg.producers_of(&consumer.id) {
        let unavailable = |why: &str| format!("producer {} {why}", d.producer);
        let Some(p) = sources.iter().find(|s| s.id == d.producer) else {
            continue;
        };
        let reason = if !p.enabled {
            Some(unavailable("is disabled"))
        } else {
            match open_producer(home, cfg, backend, p)? {
                Some(v) => {
                    producers.insert(p.id.clone(), (v, d.via));
                    None
                }
                None => Some(unavailable("has no published index")),
            }
        };
        if let Some(reason) = reason {
            manifest.state = "stale".into();
            manifest.reason = Some(reason.clone());
            persist(cfg, backend, consumer, &mut view.store, &mut manifest)?;
            return Ok(stale(reason));
        }
    }

    // A dependency missing because discovery could not complete is
    // unknown, not removed: keep the last bindings.
    if let Some(why) = reg.incomplete.get(&consumer.id) {
        if manifest
            .producers
            .keys()
            .any(|p| !producers.contains_key(p))
        {
            let reason = format!("dependency discovery incomplete: {why}");
            manifest.state = "stale".into();
            manifest.reason = Some(reason.clone());
            persist(cfg, backend, consumer, &mut view.store, &mut manifest)?;
            return Ok(stale(reason));
        }
    }

    let same_producers = manifest.producers.len() == producers.len()
        && producers
            .iter()
            .all(|(id, (_, via))| manifest.producers.get(id).is_some_and(|s| s.via == *via));
    let full = manifest.state != "current"
        || manifest.consumer_generation != graph_generation
        || !same_producers;
    // Changed lookups: only producers whose base moved are re-checked.
    let mut changed: BTreeSet<String> = BTreeSet::new();
    let mut triggered_by = BTreeSet::new();
    if !full {
        for (id, (p, _)) in &producers {
            if manifest.producers[id].base_generation == p.generation {
                continue;
            }
            let keys = manifest.lookups.get(id).cloned().unwrap_or_default();
            let names: Vec<&str> = keys
                .keys()
                .map(|k| k.strip_prefix("q:").unwrap_or(k))
                .collect();
            let found = match p.lookup(&names) {
                Ok(found) => found,
                Err(e) => {
                    let reason = lookup_failed(id, &e);
                    manifest.state = "stale".into();
                    manifest.reason = Some(reason.clone());
                    persist(cfg, backend, consumer, &mut view.store, &mut manifest)?;
                    return Ok(stale(reason));
                }
            };
            for (key, fp) in &keys {
                let qn = key.strip_prefix("q:").unwrap_or(key);
                let rows = found.get(qn).map(Vec::as_slice).unwrap_or_default();
                if candidates_fingerprint(rows) != *fp {
                    changed.insert(key.clone());
                    triggered_by.insert(id.clone());
                }
            }
        }
        if changed.is_empty() {
            // Implementation-only producer changes: record the snapshots
            // they were checked against, nothing else.
            let mut moved = false;
            for (id, (p, _)) in &producers {
                if let Some(s) = manifest.producers.get_mut(id) {
                    if s.base_generation != p.generation {
                        s.base_generation = p.generation.clone();
                        moved = true;
                    }
                }
            }
            if moved {
                // Through the authoritative path: the server must never lag
                // the staging cache.
                persist(cfg, backend, consumer, &mut view.store, &mut manifest)?;
            }
            return Ok(DependencyOutcome::Current);
        }
    }
    if full {
        triggered_by.extend(producers.keys().cloned());
    }

    // Dirty first: an interrupted update is retried by the next run.
    manifest.state = "stale".into();
    manifest.reason = Some(format!(
        "dependency inputs changed ({})",
        triggered_by.iter().cloned().collect::<Vec<_>>().join(", ")
    ));
    persist(cfg, backend, consumer, &mut view.store, &mut manifest)?;

    let refs: Vec<_> = view
        .store
        .cross_file_references(&view.build)
        .map_err(db)?
        .into_iter()
        .filter(|r| r.resolver == "unresolved")
        .filter_map(|r| Some((r.reference_text.clone()?, r)))
        .filter(|(t, _)| full || changed.contains(&qualified_key(t)))
        .collect();
    let mut lookups: BTreeMap<String, BTreeMap<String, String>> = if full {
        BTreeMap::new()
    } else {
        manifest.lookups.clone()
    };
    let mut bindings: BTreeMap<String, ExternalBinding> = if full {
        BTreeMap::new()
    } else {
        manifest
            .bindings
            .iter()
            .filter(|b| !changed.contains(&qualified_key(&b.reference_text)))
            .map(|b| (b.relationship_id.clone(), b.clone()))
            .collect()
    };
    // One batched lookup per producer over every distinct name. A failed
    // lookup is never "no candidate": the consumer keeps its last bindings
    // (persisted above), marked stale.
    let names: Vec<&str> = refs
        .iter()
        .map(|(t, _)| t.as_str())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut candidates: BTreeMap<&str, Candidates> = BTreeMap::new();
    for (id, (p, _)) in &producers {
        match p.lookup(&names) {
            Ok(found) => {
                candidates.insert(id.as_str(), found);
            }
            Err(e) => {
                let reason = lookup_failed(id, &e);
                tracing::warn!(component = "relationships", event = "producer_lookup_failed", source_id = %consumer.id, producer = %id, retryable = true, error = %e);
                manifest.reason = Some(reason.clone());
                persist(cfg, backend, consumer, &mut view.store, &mut manifest)?;
                return Ok(stale(reason));
            }
        }
    }
    for (text, r) in &refs {
        let key = qualified_key(text);
        let mut found: Vec<(String, String)> = Vec::new();
        for id in producers.keys() {
            let rows = candidates[id.as_str()]
                .get(text.as_str())
                .map(Vec::as_slice)
                .unwrap_or_default();
            lookups
                .entry(id.clone())
                .or_default()
                .insert(key.clone(), candidates_fingerprint(rows));
            found.extend(rows.iter().map(|e| (id.clone(), e.id.clone())));
        }
        if found.is_empty() {
            bindings.remove(&r.id);
            continue;
        }
        let single = (found.len() == 1).then(|| found[0].clone());
        bindings.insert(
            r.id.clone(),
            ExternalBinding {
                relationship_id: r.id.clone(),
                file_id: r.file_id.clone(),
                reference_text: text.clone(),
                target_symbol: r.target_symbol.clone(),
                producer: single.as_ref().map(|(p, _)| p.clone()),
                target_entity_id: single.map(|(_, e)| e),
                ambiguous: found.len() > 1,
            },
        );
    }

    test_hooks::before_fence(&consumer.id);
    // Fence: the consumer and every producer must still be at the snapshot
    // read above, judged by the authoritative active build (an old build
    // that is still stored is not enough).
    let mut moved = None;
    if active_build(home, backend, consumer)?.as_deref() != Some(view.active.as_str())
        || view.store.graph_state().map_err(db)?.generation != graph_generation
    {
        moved = Some("the consumer was reindexed during the update".to_owned());
    }
    for (id, (p, _)) in &producers {
        if moved.is_some() {
            break;
        }
        let source = sources.iter().find(|s| &s.id == id);
        let active = match source {
            Some(s) => active_build(home, backend, s)?,
            None => None,
        };
        let now = p.generation_now()?;
        if active.as_deref() != Some(p.active.as_str())
            || now.as_deref() != Some(p.generation.as_str())
        {
            moved = Some(format!("producer {id} was republished during the update"));
        }
    }
    if let Some(reason) = moved {
        manifest.reason = Some(reason.clone());
        persist(cfg, backend, consumer, &mut view.store, &mut manifest)?;
        return Ok(stale(reason));
    }
    let bound = bindings
        .values()
        .filter(|b| b.target_entity_id.is_some())
        .count();
    manifest = ExternalManifest {
        state: "current".into(),
        reason: None,
        consumer_generation: graph_generation,
        consumer_base_generation: match backend {
            Backend::Local => view.generation.clone(),
            Backend::Server(_) => format!("server:{}", view.active),
        },
        producers: producers
            .iter()
            .map(|(id, (p, via))| {
                (
                    id.clone(),
                    ProducerSnapshot {
                        base_generation: p.generation.clone(),
                        via: (*via).to_owned(),
                    },
                )
            })
            .collect(),
        lookups,
        bindings: bindings.into_values().collect(),
        unsupported: manifest.unsupported,
        updated_at: None,
        revision: manifest.revision,
    };
    persist(cfg, backend, consumer, &mut view.store, &mut manifest)?;
    Ok(DependencyOutcome::Updated {
        full,
        keys_changed: changed.len(),
        references: refs.len(),
        bound,
        triggered_by: triggered_by.into_iter().collect(),
    })
}

/// The dependency stage over every consumer known from `sources` (the whole
/// catalog, enabled or not), each visited once under its own lock.
pub fn run_stage(
    home: &Home,
    cfg: &docmonk_config::DocMonkConfig,
    backend: &Backend,
    sources: &[SourceRecord],
    lock_timeout: std::time::Duration,
    on_outcome: impl FnMut(&SourceRecord, DependencyOutcome),
) {
    run_stage_holding(home, cfg, backend, sources, lock_timeout, None, on_outcome)
}

/// [`run_stage`] when the caller already holds the lock of source `held`
/// (the daemon's graph pass): that consumer is updated without locking it
/// again.
pub fn run_stage_holding(
    home: &Home,
    cfg: &docmonk_config::DocMonkConfig,
    backend: &Backend,
    sources: &[SourceRecord],
    lock_timeout: std::time::Duration,
    held: Option<&str>,
    mut on_outcome: impl FnMut(&SourceRecord, DependencyOutcome),
) {
    let reg = discover(home, cfg, backend, sources);
    for d in &reg.invalid {
        tracing::warn!(component = "relationships", event = "dependency_ignored", declaration = %d);
    }
    for consumer_id in reg.refresh_candidates() {
        let Some(consumer) = sources.iter().find(|s| s.id == consumer_id) else {
            continue;
        };
        let update = || {
            update_consumer(home, cfg, backend, &reg, sources, consumer)
                .unwrap_or_else(|e| stale(e.message().to_owned()))
        };
        let outcome = if held == Some(consumer.id.as_str()) {
            update()
        } else {
            match RunLock::acquire(
                &source_lock_path(home, &consumer.id),
                "dependencies",
                Some(&consumer.id),
                lock_timeout,
            ) {
                Err(e) => stale(format!("consumer is locked: {}", e.message())),
                Ok(lock) => {
                    let o = update();
                    lock.release();
                    o
                }
            }
        };
        if matches!(
            outcome,
            DependencyOutcome::Stale { .. } | DependencyOutcome::Pending { .. }
        ) {
            tracing::warn!(component = "relationships", event = "dependencies_not_current", source_id = %consumer.id, state = outcome.state());
        }
        on_outcome(consumer, outcome);
    }
}

/// `docmonk`-level helper: the dependency stage over the whole catalog.
pub fn refresh(
    home: &Home,
    mut on_outcome: impl FnMut(&SourceRecord, &DependencyOutcome),
) -> Result<DependencySummary, DocMonkError> {
    let cfg = load(home)?;
    let backend = crate::backend::open_for_write(home)?;
    let sources = crate::sources::catalog(home)?.list(false)?;
    let opts = docmonk_indexing::coordinator::Options::from_config(&cfg);
    let mut summary = DependencySummary::default();
    run_stage(home, &cfg, &backend, &sources, opts.lock_timeout, |s, o| {
        summary.add(&o);
        on_outcome(s, &o);
    });
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csproj_project_and_package_references_are_parsed() {
        let r = csproj_references(
            r#"<Project><ItemGroup>
                <ProjectReference Include="..\Lib\Lib.csproj" />
                <PackageReference Include="Newtonsoft.Json" Version="13" />
            </ItemGroup></Project>"#,
        );
        assert_eq!(r.projects, vec![r"..\Lib\Lib.csproj".to_owned()]);
        assert_eq!(r.packages, vec!["Newtonsoft.Json".to_owned()]);
        assert!(r.unsupported.is_empty());
        assert_eq!(
            normalize(Path::new("/r/a/../b/./c.csproj")),
            PathBuf::from("/r/b/c.csproj")
        );
    }

    #[test]
    fn f14_csproj_parsing_is_xml_aware_and_never_guesses() {
        let r = csproj_references(
            r#"<?xml version="1.0"?>
<Project Sdk="Microsoft.NET.Sdk" xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
  <!-- <ProjectReference Include="..\Commented\Out.csproj" /> -->
  <Import Project="$(MSBuildThisFileDirectory)shared.props" />
  <ItemGroup>
    <ProjectReference Include='..\Single\Single.csproj' />
    <ProjectReference
        Include="..\Multi\Multi.csproj">
      <Private>false</Private>
    </ProjectReference>
    <ProjectReference Include="..\A\A.csproj;..\B\B.csproj" />
    <ProjectReference Include="$(Root)\Prop.csproj" />
    <ProjectReference Include="..\Glob\*.csproj" />
  </ItemGroup>
  <ItemGroup Condition="'$(Configuration)' == 'Debug'">
    <ProjectReference Include="..\Debug\Debug.csproj" />
  </ItemGroup>
</Project>"#,
        );
        assert_eq!(
            r.projects,
            [
                r"..\Single\Single.csproj",
                r"..\Multi\Multi.csproj",
                r"..\A\A.csproj",
                r"..\B\B.csproj"
            ]
        );
        assert!(!r.projects.iter().any(|p| p.contains("Commented")));
        let u = r.unsupported.join("\n");
        for want in [
            "shared.props",
            "Prop.csproj",
            "Glob",
            "Debug.csproj is conditional",
        ] {
            assert!(u.contains(want), "{want}: {u}");
        }
        assert_eq!(csproj_references("<Project").unsupported.len(), 1);
    }

    #[test]
    fn nested_source_roots_resolve_to_the_innermost() {
        let rec = |id: &str, path: &str| SourceRecord {
            id: id.into(),
            path: path.into(),
            source_type: docmonk_core::models::SourceType::Local,
            enabled: true,
            include_patterns: vec![],
            exclude_patterns: vec![],
            created_at: String::new(),
            updated_at: String::new(),
        };
        let sources = [rec("outer", "/r"), rec("inner", "/r/lib")];
        let hit = containing_source(&sources, Path::new("/r/lib/L.csproj")).unwrap();
        assert_eq!(hit.id, "inner");
        assert_eq!(
            containing_source(&sources, Path::new("/x/y")).map(|s| &s.id),
            None
        );
    }
}
