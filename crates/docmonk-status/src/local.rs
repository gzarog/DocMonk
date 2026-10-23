//! Local-mode collector: the home's SQLite catalog, each source's
//! published build and error log, and this home's runs. Nothing else is
//! consulted (no server, ever).
//!
//! Per snapshot: one statement for the catalog, one for every source
//! state, and per source with a published build one connection with two
//! statements (the build summary and the newest errors). No per-file
//! queries.

use chrono::{DateTime, Utc};
use docmonk_core::paths::{project_id_for_canonical, Home};
use docmonk_storage::control::{BuildState as StoredBuild, ControlPlane};
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::StorageLayout;

use crate::model::*;
use crate::snapshot::{assemble, normalize_time, CollectOptions, Collected, SourceFacts};

/// Inputs of a local snapshot.
pub struct LocalInputs<'a> {
    pub home: &'a Home,
    pub control: &'a ControlPlane,
    pub cache_size_mb: i64,
}

fn db_err(e: impl std::fmt::Display) -> CollectError {
    CollectError::Database(e.to_string())
}

pub(crate) fn build_state(s: StoredBuild, has_active: bool) -> BuildState {
    match s {
        StoredBuild::Ready => BuildState::Ready,
        StoredBuild::Building => BuildState::Building,
        StoredBuild::Failed => BuildState::Failed,
        StoredBuild::NeedsFullRebuild if has_active => BuildState::NeedsFullRebuild,
        StoredBuild::NeedsFullRebuild => BuildState::NotIndexed,
    }
}

fn u(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

/// Collects the facts of a local snapshot.
pub fn collect_facts(
    i: &LocalInputs<'_>,
    opts: &CollectOptions,
    now: DateTime<Utc>,
) -> Result<Collected, CollectError> {
    let layout = StorageLayout::new(i.home);
    let mut statements: u64 = 0;
    let catalog = i.control.list_sources(false).map_err(db_err)?;
    let states = i.control.all_states().map_err(db_err)?;
    statements += 2;
    let limit = opts.error_limit() as i64;
    let mut sources = Vec::with_capacity(catalog.len());
    let mut errors = Vec::new();
    let mut missing = Vec::new();
    for s in &catalog {
        let st = states.iter().find(|x| x.source_id == s.id);
        let active = st.and_then(|x| x.active_build_id.clone());
        let access = if !s.enabled {
            Access::Disabled
        } else {
            match st.map(|x| x.online_status.as_str()) {
                Some("offline") => Access::Offline,
                Some(_) => Access::Online,
                None => Access::Unknown,
            }
        };
        let mut facts = SourceFacts {
            source_id: s.id.clone(),
            path: s.path.clone(),
            source_type: s.source_type.as_str().into(),
            enabled: s.enabled,
            access,
            build_state: st.map_or(BuildState::Unknown, |x| {
                build_state(x.build_state, active.is_some())
            }),
            active_build_id: active.clone(),
            pending_build_id: st.and_then(|x| x.pending_build_id.clone()),
            published_at: None,
            last_scan_at: normalize_time(st.and_then(|x| x.last_scan_at.as_deref())),
            last_error: st.and_then(|x| x.last_error.clone()),
            last_error_at: None,
            published: None,
            published_missing: false,
            lease: None,
            relationships: None,
            dependencies: None,
        };
        let project = project_id_for_canonical(&s.path);
        let db = layout.project_db(&project);
        if db.exists() {
            let wanted = opts.error_source.as_deref().is_none_or(|f| f == s.id);
            match read_project(
                &layout,
                &project,
                &s.id,
                active.as_deref(),
                limit,
                wanted,
                i.cache_size_mb,
            ) {
                Ok((summary, recent, graph, deps, n)) => {
                    statements += n;
                    facts.dependencies = deps;
                    let visible = graph.as_ref().is_some_and(|g| g.state == "ready");
                    facts.relationships = graph;
                    if let Some(mut b) = summary {
                        if !visible {
                            // Graph rows of another base generation are
                            // not part of what readers see.
                            b.relationships = 0;
                            b.links = 0;
                        }
                        facts.published_at = normalize_time(b.published_at.as_deref());
                        facts.published = Some(PublishedCounts {
                            files: u(b.files),
                            indexed: u(b.indexed),
                            failed: u(b.failed),
                            retrying: u(b.retry),
                            code_entities: u(b.entities),
                            documents: u(b.documents),
                            chunks: u(b.chunks),
                            passages: u(b.passages),
                            degraded: u(b.degraded),
                            relationships: u(b.relationships),
                            links: u(b.links),
                            max_attempt_count: u(b.max_attempt_count),
                            next_retry_at: normalize_time(b.next_retry_at.as_deref()),
                        });
                    }
                    facts.last_error_at = recent.first().and_then(|e| e.occurred_at.clone());
                    if wanted {
                        errors.extend(recent);
                    }
                }
                Err(e) => {
                    // A source's store that cannot be read is unknown, never zero.
                    facts.published_missing = true;
                    missing.push(format!("published:{} ({e})", s.id));
                }
            }
        } else if active.is_some() {
            facts.published_missing = true;
            missing.push(format!("published:{} (project store missing)", s.id));
        }
        sources.push(facts);
    }
    let ids: Vec<String> = catalog.iter().map(|s| s.id.clone()).collect();
    let rt = crate::runtime::read_local(i.home, &ids, opts.stall_threshold_seconds, now);
    Ok(Collected {
        mode: Mode::Local,
        backend: BackendInfo {
            kind: BackendKind::Sqlite,
            index_prefix: None,
            endpoint: None,
            reachable: true,
            authoritative: true,
            cluster: None,
            latency_ms: None,
        },
        sources,
        runs: rt.runs,
        scope: IndexerScope::Host,
        max_parallel_sources: rt.max_parallel_sources,
        resources: rt.resources,
        lock: rt.lock,
        last_run: rt.last_run,
        errors: Some(errors),
        missing_sections: missing,
        request_count: Some(statements),
        cached_sections: Vec::new(),
        consistency: Consistency::Consistent,
        inconsistent_sources: Vec::new(),
    })
}

type ProjectRead = (
    Option<docmonk_storage::status::BuildSummary>,
    Vec<ErrorEvent>,
    Option<RelationshipStatus>,
    Option<crate::model::DependencyStatus>,
    u64,
);

/// One connection: the published build's summary and the newest errors.
fn read_project(
    layout: &StorageLayout,
    project: &str,
    source_id: &str,
    active: Option<&str>,
    limit: i64,
    want_errors: bool,
    cache_size_mb: i64,
) -> Result<ProjectRead, CollectError> {
    let store = ProjectStore::open(layout, project, source_id, cache_size_mb).map_err(db_err)?;
    let mut n = 0;
    let summary = match active {
        Some(b) => {
            n += 1;
            Some(store.build_summary(b).map_err(db_err)?)
        }
        None => None,
    };
    let limit = if want_errors { limit } else { 1 };
    n += 1;
    let recent = store
        .recent_errors(limit)
        .map_err(db_err)?
        .into_iter()
        .map(|e| ErrorEvent {
            source_id: source_id.to_owned(),
            path: e.path,
            code: e.error_code,
            message: e.error_message,
            status: None,
            attempt_count: None,
            occurred_at: normalize_time(Some(&e.occurred_at)),
        })
        .collect();
    // The graph record came with the build summary (no extra statement).
    let relationships = match (active, &summary) {
        (Some(b), Some(sum)) => {
            let graph = ProjectStore::graph_status_of(b, sum);
            Some(RelationshipStatus {
                state: graph.state.as_str().into(),
                generation: (graph.generation > 0).then(|| graph.generation.to_string()),
                base_build_id: graph.base_build_id,
                last_success_at: normalize_time(graph.last_success_at.as_deref()),
                last_error: graph.last_error,
                stale_reason: graph.stale_reason,
            })
        }
        _ => None,
    };
    // The consumer's dependency manifest (one metadata row).
    n += 1;
    let dependencies = store.external_manifest().map_err(db_err)?.and_then(|m| {
        crate::snapshot::dependency_status(
            Some(m.state.as_str()),
            m.reason.as_deref(),
            m.updated_at.as_deref(),
            m.producers.keys().cloned().collect(),
        )
    });
    Ok((summary, recent, relationships, dependencies, n))
}

/// A complete local report.
pub fn collect(i: &LocalInputs<'_>, opts: &CollectOptions) -> Result<StatusReport, CollectError> {
    let started = std::time::Instant::now();
    let now = Utc::now();
    let c = collect_facts(i, opts, now)?;
    Ok(assemble(
        c,
        opts,
        now,
        Some(started.elapsed().as_millis() as u64),
    ))
}
