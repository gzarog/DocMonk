//! Server-mode collector: everything comes from the configured
//! OpenSearch/Elasticsearch prefix through one batched
//! [`ServerBackend::status_snapshot`]. No local store is opened.
//!
//! A [`ServerSession`] makes `--watch` cheap: each refresh re-reads only
//! the catalog, run heartbeats and cluster health (3 requests); the heavy
//! published aggregates and error feed are reused while the set of active
//! builds is unchanged and younger than the session's TTL, and re-read on
//! any publication. Cached sections and their age are listed in the
//! report's diagnostics.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use chrono::Utc;
use docmonk_backends::status::{FileErrorEvent, PublishedAgg, StatusQuery};
use docmonk_backends::{BackendError, ServerBackend};
use serde_json::Value;

use crate::model::*;
use crate::snapshot::{
    assemble, normalize_time, parse_time, CollectOptions, Collected, LeaseFacts, SourceFacts,
};

/// Default lifetime of cached published aggregates in a watch session.
pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(30);

/// What a server snapshot needs besides the backend.
pub struct ServerInputs<'a> {
    pub backend: &'a ServerBackend,
    /// The configured URL (credentials are stripped before reporting).
    pub endpoint: &'a str,
}

struct Cached {
    pairs: BTreeMap<String, String>,
    at: Instant,
    published: BTreeMap<String, PublishedAgg>,
    errors: Vec<FileErrorEvent>,
}

/// Reuses published aggregates between refreshes of one watch.
pub struct ServerSession {
    ttl: Duration,
    cached: Option<Cached>,
}

impl ServerSession {
    pub fn new(ttl: Duration) -> Self {
        Self { ttl, cached: None }
    }
}

impl Default for ServerSession {
    fn default() -> Self {
        Self::new(DEFAULT_CACHE_TTL)
    }
}

/// A backend error as a collection error class.
pub fn collect_error(e: BackendError) -> CollectError {
    let msg = docmonk_telemetry::redact::redact_urls_in_text(&e.to_string());
    match e {
        BackendError::Transport(_) => CollectError::Unavailable(msg),
        BackendError::SchemaMismatch(_) => CollectError::SchemaMismatch(msg),
        BackendError::Invalid(_) => CollectError::Config(msg),
        BackendError::Http { status: 404, .. } => CollectError::SchemaMismatch(format!(
            "{msg}; run `docmonk server init` (or use a fresh index_prefix)"
        )),
        _ => CollectError::Unavailable(msg),
    }
}

fn s<'a>(d: &'a Value, k: &str) -> Option<&'a str> {
    d.get(k).and_then(Value::as_str).filter(|v| !v.is_empty())
}

fn build_state(state: Option<&str>, has_active: bool) -> BuildState {
    match state {
        Some("ready") => BuildState::Ready,
        Some("building") => BuildState::Building,
        Some("failed") => BuildState::Failed,
        Some("needs_full_rebuild") if has_active => BuildState::NeedsFullRebuild,
        Some("needs_full_rebuild") => BuildState::NotIndexed,
        _ => BuildState::Unknown,
    }
}

fn published(a: &PublishedAgg) -> PublishedCounts {
    PublishedCounts {
        files: a.files,
        indexed: a.indexed,
        failed: a.failed,
        retrying: a.retry,
        code_entities: a.entities,
        documents: a.documents,
        chunks: a.chunks,
        relationships: a.relationships,
        links: a.links,
        max_attempt_count: a.max_attempt_count,
        next_retry_at: normalize_time(a.next_retry_at.as_deref()),
    }
}

fn lease_of(d: &Value, now: chrono::DateTime<Utc>) -> Option<LeaseFacts> {
    let owner = s(d, "lease_owner")?;
    let expires = s(d, "lease_expires_at");
    Some(LeaseFacts {
        owner: owner.to_owned(),
        token: d.get("lease_token").and_then(Value::as_i64).unwrap_or(0),
        expires_at: normalize_time(expires),
        live: expires.and_then(parse_time).is_some_and(|e| e > now),
    })
}

/// Collects a server snapshot; `session` enables the watch cache.
pub fn collect(
    i: &ServerInputs<'_>,
    opts: &CollectOptions,
    session: Option<&mut ServerSession>,
) -> Result<StatusReport, CollectError> {
    let started = Instant::now();
    let now = Utc::now();

    let query = |published| StatusQuery {
        error_limit: opts.error_limit(),
        error_source: opts.error_source.clone(),
        published,
    };
    // A fresh cache lets this refresh skip the heavy sections.
    let fresh_cache = session
        .as_deref()
        .and_then(|s| s.cached.as_ref().map(|c| (c, s.ttl)))
        .filter(|(c, ttl)| c.at.elapsed() < *ttl);
    let mut snap = i
        .backend
        .status_snapshot(&query(fresh_cache.is_none()))
        .map_err(collect_error)?;
    let mut cached_sections = Vec::new();
    if let Some((cached, ttl)) = fresh_cache {
        if cached.pairs == snap.pairs {
            let age = (cached.at.elapsed().as_secs_f64() * 10.0).round() / 10.0;
            snap.published = Some(Ok(cached.published.clone()));
            snap.errors = Some(Ok(cached.errors.clone()));
            for section in ["published", "recent_errors"] {
                cached_sections.push(CachedSection {
                    section: section.into(),
                    age_seconds: age,
                    ttl_seconds: ttl.as_secs_f64(),
                });
            }
        } else {
            // A publication happened: read the heavy sections now.
            let cheap = snap.requests;
            snap = i
                .backend
                .status_snapshot(&query(true))
                .map_err(collect_error)?;
            snap.requests += cheap;
        }
    }
    if let (Some(sess), true) = (session, cached_sections.is_empty()) {
        if let (Some(Ok(p)), Some(Ok(e))) = (&snap.published, &snap.errors) {
            sess.cached = Some(Cached {
                pairs: snap.pairs.clone(),
                at: Instant::now(),
                published: p.clone(),
                errors: e.clone(),
            });
        }
    }

    let mut missing = Vec::new();
    let published_map = match snap.published {
        Some(Ok(p)) => Some(p),
        Some(Err(e)) => {
            missing.push(format!("published ({e})"));
            None
        }
        None => None,
    };
    let mut sources = Vec::with_capacity(snap.states.len());
    let mut leases = BTreeMap::new();
    for d in &snap.states {
        let Some(id) = s(d, "source_id") else {
            continue;
        };
        let enabled = d.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        let active = snap.pairs.get(id).cloned();
        let lease = lease_of(d, now);
        if let Some(l) = &lease {
            leases.insert(id.to_owned(), l.clone());
        }
        let agg = published_map.as_ref().and_then(|m| m.get(id));
        sources.push(SourceFacts {
            source_id: id.to_owned(),
            path: s(d, "path").unwrap_or_default().to_owned(),
            source_type: s(d, "source_type").unwrap_or("local").to_owned(),
            enabled,
            access: if !enabled {
                Access::Disabled
            } else {
                match s(d, "online_status") {
                    Some("offline") => Access::Offline,
                    Some("online") => Access::Online,
                    _ => Access::Unknown,
                }
            },
            build_state: build_state(s(d, "state"), active.is_some()),
            pending_build_id: s(d, "pending_build_id").map(str::to_owned),
            published_at: normalize_time(s(d, "published_at")),
            last_scan_at: normalize_time(s(d, "last_scan_at")),
            last_error: s(d, "last_error").map(str::to_owned),
            last_error_at: agg.and_then(|a| normalize_time(a.last_error_at.as_deref())),
            published: active.as_ref().and(agg).map(published),
            published_missing: active.is_some() && published_map.is_none(),
            relationships: Some(relationship_status(d, active.as_deref())),
            dependencies: dependency_status(d),
            active_build_id: active,
            lease,
        });
    }
    let errors = match snap.errors {
        Some(Ok(list)) => Some(
            list.into_iter()
                .map(|e| ErrorEvent {
                    source_id: e.source_id,
                    path: Some(e.rel_path),
                    code: e.status.clone().unwrap_or_else(|| "error".into()),
                    message: e.error,
                    status: e.status,
                    attempt_count: e.attempt_count,
                    occurred_at: normalize_time(e.last_error_at.as_deref()),
                })
                .collect(),
        ),
        Some(Err(e)) => {
            missing.push(format!("recent_errors ({e})"));
            None
        }
        None => Some(Vec::new()),
    };
    let runs = match &snap.runtime {
        Ok(docs) => crate::runtime::server_runs(docs, &leases, opts.stall_threshold_seconds, now),
        Err(e) => {
            missing.push(format!("runtime ({e})"));
            Vec::new()
        }
    };
    let cluster = match &snap.cluster {
        Ok(c) => ClusterInfo {
            health: match c.status.as_deref() {
                Some("green") => Some(ClusterHealth::Green),
                Some("yellow") => Some(ClusterHealth::Yellow),
                Some("red") => Some(ClusterHealth::Red),
                _ => None,
            },
            unassigned_shards: c.unassigned_shards,
            number_of_nodes: c.number_of_nodes,
            indexes: c.indexes.clone(),
        },
        Err(_) => ClusterInfo {
            health: None,
            unassigned_shards: None,
            number_of_nodes: None,
            indexes: Vec::new(),
        },
    };
    let consistency = if !snap.inconsistent.is_empty() {
        Consistency::Inconsistent
    } else if !snap.retried.is_empty() {
        Consistency::Retried
    } else {
        Consistency::Consistent
    };
    let kind = match i.backend.engine().as_str() {
        "elasticsearch" => BackendKind::Elasticsearch,
        _ => BackendKind::Opensearch,
    };
    let c = Collected {
        mode: Mode::Server,
        backend: BackendInfo {
            kind,
            index_prefix: Some(i.backend.prefix().to_owned()),
            endpoint: Some(docmonk_telemetry::redact::redact_url(i.endpoint)),
            reachable: true,
            authoritative: true,
            cluster: Some(cluster),
            latency_ms: Some(snap.latency_ms),
        },
        sources,
        runs,
        scope: IndexerScope::Cluster,
        max_parallel_sources: None,
        resources: None,
        lock: None,
        last_run: None,
        errors,
        missing_sections: missing,
        request_count: Some(snap.requests),
        cached_sections,
        consistency,
        inconsistent_sources: snap.inconsistent,
    };
    Ok(assemble(
        c,
        opts,
        now,
        Some(started.elapsed().as_millis() as u64),
    ))
}

/// The dependency manifest of a source-state document
/// (`versions.graph_external`, bindings excluded from the fetch).
fn dependency_status(d: &Value) -> Option<crate::model::DependencyStatus> {
    let m = &d["versions"][docmonk_backends::graph::EXTERNAL_KEY];
    let producers = m["producers"]
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    crate::snapshot::dependency_status(
        m["state"].as_str(),
        m["reason"].as_str(),
        m["updated_at"].as_str(),
        producers,
    )
}

/// The graph record of a source-state document (`versions.graph`).
fn relationship_status(d: &Value, active: Option<&str>) -> RelationshipStatus {
    let g = &d["versions"][docmonk_backends::graph::GRAPH_KEY];
    let get = |k: &str| g.get(k).and_then(Value::as_str).map(str::to_owned);
    let base = get("base_build_id");
    let mut state = get("state").unwrap_or_else(|| "pending".into());
    let mut stale_reason = get("stale_reason");
    if state == "ready" && base.as_deref() != active {
        state = "stale".into();
        stale_reason.get_or_insert_with(|| "the base index was republished".into());
    }
    RelationshipStatus {
        state,
        generation: get("generation"),
        base_build_id: base,
        last_success_at: normalize_time(get("published_at").as_deref()),
        last_error: get("last_error"),
        stale_reason,
    }
}
