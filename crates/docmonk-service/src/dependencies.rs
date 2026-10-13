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
//! * Fencing: before the manifest is promoted, every producer's base
//!   generation is re-read; a producer republished meanwhile leaves the
//!   consumer `stale`. A consumer is marked `stale` before it is updated, so
//!   an interrupted run is picked up by the next one; unavailable producers
//!   or consumers are reported `stale`/`pending`, never silently current.
//! * Runs indexing only some sources still update their impacted consumers
//!   against those consumers' existing published bases.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use docmonk_config::model::parse_dependency;
use docmonk_core::errors::RagMonkError;
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
}

/// A source's published build, opened for reading.
pub(crate) struct View {
    pub store: ProjectStore,
    pub build: String,
    pub generation: String,
}

pub(crate) fn open_view(
    home: &Home,
    cfg: &docmonk_config::RagMonkConfig,
    backend: &Backend,
    source: &SourceRecord,
) -> Result<Option<View>, RagMonkError> {
    let (store, build) = match backend {
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
            (store, build)
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
    }))
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

/// `(project references, package references)` of a `.csproj` document.
pub fn csproj_references(text: &str) -> (Vec<String>, Vec<String>) {
    let attr = |tag: &str| -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = text;
        while let Some(i) = rest.find(tag) {
            rest = &rest[i + tag.len()..];
            let end = rest.find('>').unwrap_or(rest.len());
            let item = &rest[..end];
            if let Some(j) = item.find("Include=\"") {
                let v = &item[j + 9..];
                if let Some(k) = v.find('"') {
                    out.push(v[..k].to_owned());
                }
            }
        }
        out
    };
    (attr("<ProjectReference"), attr("<PackageReference"))
}

/// Collects the declared and discovered dependencies of `sources`.
pub fn discover(
    home: &Home,
    cfg: &docmonk_config::RagMonkConfig,
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
    for source in sources {
        let Ok(Some(view)) = open_view(home, cfg, backend, source) else {
            continue;
        };
        let Ok(projects) = view.store.file_paths_with_suffix(&view.build, ".csproj") else {
            continue;
        };
        for rel_path in &projects {
            let path = Path::new(&source.path).join(rel_path);
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let (projects, packages) = csproj_references(&text);
            let dir = path.parent().unwrap_or(Path::new(&source.path));
            for r in projects {
                let target = normalize(&dir.join(r.replace('\\', "/")));
                let producer = sources
                    .iter()
                    .find(|s| s.id != source.id && target.starts_with(Path::new(&s.path)));
                match producer {
                    Some(p) => {
                        reg.edges.insert(Dependency {
                            consumer: source.id.clone(),
                            producer: p.id.clone(),
                            via: "project_reference",
                        });
                    }
                    None if target.starts_with(Path::new(&source.path)) => {}
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

fn stale(reason: impl Into<String>) -> DependencyOutcome {
    DependencyOutcome::Stale {
        reason: reason.into(),
    }
}

/// Records `m` on the consumer (and on the server, the authoritative copy).
fn persist(
    cfg: &docmonk_config::RagMonkConfig,
    backend: &Backend,
    consumer: &SourceRecord,
    store: &mut ProjectStore,
    m: &ExternalManifest,
) -> Result<(), RagMonkError> {
    store.put_external_manifest(m).map_err(db)?;
    if let Backend::Server(server) = backend {
        let ttl = std::time::Duration::from_secs_f64(cfg.storage.server.lease_seconds);
        let lease = server
            .acquire_lease(&consumer.id, &lease_owner(), ttl)
            .map_err(server_err)?;
        let value = serde_json::to_value(m).map_err(crate::generic)?;
        let r = server
            .set_graph_external(&consumer.id, Some(&lease), &value)
            .map_err(server_err);
        let _ = server.release_lease(&lease);
        r?;
    }
    Ok(())
}

/// Updates one consumer's bindings. The caller holds its lock.
fn update_consumer(
    home: &Home,
    cfg: &docmonk_config::RagMonkConfig,
    backend: &Backend,
    reg: &Registry,
    sources: &[SourceRecord],
    consumer: &SourceRecord,
) -> Result<DependencyOutcome, RagMonkError> {
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
    let mut manifest = match view.store.external_manifest().map_err(db)? {
        Some(m) => m,
        None => match backend {
            // The authoritative copy survives a lost staging cache.
            Backend::Server(server) => server
                .graph_external(&consumer.id)
                .map_err(server_err)?
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default(),
            Backend::Local => ExternalManifest::default(),
        },
    };
    manifest.unsupported = reg
        .unsupported
        .get(&consumer.id)
        .cloned()
        .unwrap_or_default();
    if !view.store.graph_visible(&view.build).map_err(db)? {
        if manifest.state != "stale" {
            manifest.state = "stale".into();
            manifest.reason = Some("the consumer's own graph is not current".into());
            persist(cfg, backend, consumer, &mut view.store, &manifest)?;
        }
        return Ok(stale("the consumer's own graph is not current"));
    }
    let graph_generation = view.store.graph_state().map_err(db)?.generation;

    // Producers in scope, each opened at its current published base.
    let mut producers: BTreeMap<String, (View, &'static str)> = BTreeMap::new();
    for d in reg.producers_of(&consumer.id) {
        let unavailable = |why: &str| format!("producer {} {why}", d.producer);
        let Some(p) = sources.iter().find(|s| s.id == d.producer) else {
            continue;
        };
        let reason = if !p.enabled {
            Some(unavailable("is disabled"))
        } else {
            match open_view(home, cfg, backend, p)? {
                Some(v) => {
                    producers.insert(p.id.clone(), (v, d.via));
                    None
                }
                None => Some(unavailable("has no published index on this host")),
            }
        };
        if let Some(reason) = reason {
            manifest.state = "stale".into();
            manifest.reason = Some(reason.clone());
            persist(cfg, backend, consumer, &mut view.store, &manifest)?;
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
            for (key, fp) in manifest.lookups.get(id).into_iter().flatten() {
                let qn = key.strip_prefix("q:").unwrap_or(key);
                let rows = p
                    .store
                    .entities_by_qualified_name(&p.build, qn)
                    .map_err(db)?;
                if candidates_fingerprint(&rows) != *fp {
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
                view.store.put_external_manifest(&manifest).map_err(db)?;
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
    persist(cfg, backend, consumer, &mut view.store, &manifest)?;

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
    let mut cache: BTreeMap<(String, String), Vec<docmonk_storage::knowledge::EntityRow>> =
        BTreeMap::new();
    for (text, r) in &refs {
        let key = qualified_key(text);
        let mut found: Vec<(String, String)> = Vec::new();
        for (id, (p, _)) in &producers {
            let rows = cache.entry((id.clone(), text.clone())).or_insert_with(|| {
                p.store
                    .entities_by_qualified_name(&p.build, text)
                    .unwrap_or_default()
            });
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
                producer: single.as_ref().map(|(p, _)| p.clone()),
                target_entity_id: single.map(|(_, e)| e),
                ambiguous: found.len() > 1,
            },
        );
    }

    // Fence: every producer must still be at the snapshot read above.
    for (id, (p, _)) in &producers {
        let now = p.store.base_generation(&p.build).map_err(db)?;
        if now.as_deref() != Some(p.generation.as_str()) {
            let reason = format!("producer {id} was republished during the update");
            manifest.reason = Some(reason.clone());
            persist(cfg, backend, consumer, &mut view.store, &manifest)?;
            return Ok(stale(reason));
        }
    }
    let bound = bindings
        .values()
        .filter(|b| b.target_entity_id.is_some())
        .count();
    manifest = ExternalManifest {
        state: "current".into(),
        reason: None,
        consumer_generation: graph_generation,
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
    };
    persist(cfg, backend, consumer, &mut view.store, &manifest)?;
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
    cfg: &docmonk_config::RagMonkConfig,
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
    cfg: &docmonk_config::RagMonkConfig,
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
    for consumer_id in reg.consumers() {
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

/// `ragmonk`-level helper: the dependency stage over the whole catalog.
pub fn refresh(
    home: &Home,
    mut on_outcome: impl FnMut(&SourceRecord, &DependencyOutcome),
) -> Result<DependencySummary, RagMonkError> {
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
        let (p, k) = csproj_references(
            r#"<Project><ItemGroup>
                <ProjectReference Include="..\Lib\Lib.csproj" />
                <PackageReference Include="Newtonsoft.Json" Version="13" />
            </ItemGroup></Project>"#,
        );
        assert_eq!(p, vec![r"..\Lib\Lib.csproj".to_owned()]);
        assert_eq!(k, vec!["Newtonsoft.Json".to_owned()]);
        assert_eq!(
            normalize(Path::new("/r/a/../b/./c.csproj")),
            PathBuf::from("/r/b/c.csproj")
        );
    }
}
