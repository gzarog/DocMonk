//! Server collector through a deterministic mock transport: request
//! budgets (150 idle sources ≤ 30 requests, a watch refresh ≤ 10),
//! publication races, paging past 10,000 sources and sections that fail.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use docmonk_backends::bulk::BulkLimits;
use docmonk_backends::engine::Engine;
use docmonk_backends::error::Result as BResult;
use docmonk_backends::transport::{Counting, Method, Response, Transport};
use docmonk_backends::{BackendError, ServerBackend};
use docmonk_status::model::*;
use docmonk_status::server::{collect, ServerInputs, ServerSession};
use docmonk_status::{CollectError, CollectOptions};
use serde_json::{json, Value};

const P: &str = "rm";

#[derive(Default)]
struct World {
    /// source_id -> active build (None: never published).
    builds: BTreeMap<String, Option<String>>,
    /// Publish this source's next build right after the first catalog read.
    publish_mid_snapshot: Option<String>,
    fail_runtime: bool,
    unreachable: bool,
}

#[derive(Clone)]
struct Mock {
    world: Arc<Mutex<World>>,
    catalog_reads: Arc<AtomicUsize>,
    published_once: Arc<AtomicBool>,
}

fn ok(v: Value) -> BResult<Response> {
    Ok(Response {
        status: 200,
        body: serde_json::to_vec(&v).unwrap(),
    })
}

fn files_for(build: &str) -> u64 {
    // Deterministic per build: 10 files, the second generation 12.
    if build.ends_with("-2") {
        12
    } else {
        10
    }
}

impl Transport for Mock {
    fn send(&self, method: Method, path: &str, body: Option<(&[u8], &str)>) -> BResult<Response> {
        let mut w = self.world.lock().unwrap();
        if w.unreachable {
            return Err(BackendError::Transport("connection refused".into()));
        }
        let b: Value = body
            .map(|(b, _)| serde_json::from_slice(b).unwrap())
            .unwrap_or(Value::Null);
        let base = path.split('?').next().unwrap();
        if method == Method::Get && base.starts_with("/_cluster/health/") {
            return ok(json!({"status": "green", "unassigned_shards": 0, "number_of_nodes": 3}));
        }
        if base == format!("/{P}-source-state/_search") {
            let n = self.catalog_reads.fetch_add(1, Ordering::SeqCst);
            let only: Option<Vec<String>> = b
                .pointer("/query/terms/source_id")
                .and_then(Value::as_array)
                .map(|a| a.iter().map(|v| v.as_str().unwrap().to_owned()).collect());
            let after = b
                .pointer("/search_after/0")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let size = b["size"].as_u64().unwrap() as usize;
            let hits: Vec<Value> = w
                .builds
                .iter()
                .filter(|(id, _)| only.as_ref().is_none_or(|o| o.contains(id)))
                .filter(|(id, _)| after.as_ref().is_none_or(|a| *id > a))
                .take(size)
                .map(|(id, build)| {
                    json!({"_source": {
                        "source_id": id, "path": format!("/srv/{id}"), "source_type": "local",
                        "enabled": true, "online_status": "online",
                        "state": if build.is_some() { "ready" } else { "needs_full_rebuild" },
                        "active_build_id": build, "published_at": "1791471600000",
                        "last_scan_at": "1791471600000",
                        // Each published build has a ready graph generation.
                        "versions": { "graph": build.as_ref().map(|b| json!({
                            "state": "ready", "generation": format!("g-{b}"), "base_build_id": b,
                        })) },
                    }, "sort": [id]})
                })
                .collect();
            if n == 0 {
                if let Some(src) = w.publish_mid_snapshot.take() {
                    w.builds.insert(src.clone(), Some(format!("b-{src}-2")));
                    self.published_once.store(true, Ordering::SeqCst);
                }
            }
            return ok(json!({"hits": {"hits": hits}}));
        }
        if base == format!("/{P}-runtime/_search") {
            if w.fail_runtime {
                return Ok(Response {
                    status: 500,
                    body: b"boom".to_vec(),
                });
            }
            return ok(json!({"hits": {"hits": []}}));
        }
        let pairs: Vec<(String, String)> = {
            let f = b
                .pointer("/query/bool/filter/0/bool/filter")
                .cloned()
                .unwrap_or_default();
            let srcs = f[0]["terms"]["source_id"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let blds = f[1]["terms"]["build_id"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            srcs.iter()
                .zip(blds.iter())
                .map(|(s, b)| {
                    (
                        s.as_str().unwrap().to_owned(),
                        b.as_str().unwrap().to_owned(),
                    )
                })
                .collect()
        };
        if base == format!("/{P}-files/_search") && b.get("aggs").is_some() {
            let buckets: Vec<Value> = pairs
                .iter()
                .map(|(s, bl)| {
                    let n = files_for(bl);
                    json!({"key": s, "doc_count": n,
                        "by_status": {"buckets": [
                            {"key": "indexed", "doc_count": n - 2},
                            {"key": "failed", "doc_count": 1},
                            {"key": "retry", "doc_count": 1}]},
                        "max_attempt": {"value": 2.0},
                        "last_error_at": {"value": 1791471600000.0},
                        "retry": {"next": {"buckets": [{"key": "2026-10-08T16:00:00+00:00"}]}}})
                })
                .collect();
            return ok(json!({"aggregations": {"by_source": {"buckets": buckets}}}));
        }
        if base == format!("/{P}-files/_search") {
            let size = b["size"].as_u64().unwrap() as usize;
            let hits: Vec<Value> = pairs
                .iter()
                .enumerate()
                .take(size)
                .map(|(i, (s, _))| {
                    json!({"_source": {"source_id": s, "rel_path": "bad.pdf", "status": "failed",
                        "last_error": "boom", "attempt_count": 2,
                        "last_error_at": (1791471600000u64 + i as u64 * 1000).to_string()}})
                })
                .collect();
            return ok(json!({"hits": {"hits": hits}}));
        }
        if base == format!("/{P}-relationships/_search") {
            // Graph generation counts.
            let srcs: Vec<Value> = pairs
                .iter()
                .map(|(s, g)| {
                    assert!(g.starts_with("g-"), "graph counts read a base build: {g}");
                    json!({"key": s, "doc_count": 7, "kind": {"buckets": [
                        {"key": "edge", "doc_count": 5}, {"key": "link", "doc_count": 2}]}})
                })
                .collect();
            return ok(json!({"aggregations": {"by_source": {"buckets": srcs}}}));
        }
        if base.contains(&format!("{P}-code,")) {
            let names = ["code", "documents", "chunks", "relationships"];
            let by_index: Vec<Value> = names
                .iter()
                .map(|n| {
                    let srcs: Vec<Value> = pairs
                        .iter()
                        .map(|(s, _)| {
                            json!({"key": s, "doc_count": 7,
                                "kind": {"buckets": if *n == "relationships" {
                                    json!([{"key": "edge", "doc_count": 5}, {"key": "link", "doc_count": 2}])
                                } else { json!([]) }}})
                        })
                        .collect();
                    json!({"key": format!("{P}-{n}"), "by_source": {"buckets": srcs}})
                })
                .collect();
            return ok(json!({"aggregations": {"by_index": {"buckets": by_index}}}));
        }
        panic!("unexpected request {method:?} {path} {b}");
    }
}

fn setup(n: usize) -> (ServerBackend, Mock) {
    let mut w = World::default();
    for i in 0..n {
        let id = format!("src_{i:05}");
        let build = (i % 50 != 49).then(|| format!("b-{id}-1"));
        w.builds.insert(id, build);
    }
    let mock = Mock {
        world: Arc::new(Mutex::new(w)),
        catalog_reads: Arc::new(AtomicUsize::new(0)),
        published_once: Arc::new(AtomicBool::new(false)),
    };
    let backend = ServerBackend::with_client(
        Counting::new(Box::new(mock.clone())),
        Engine::OpenSearch,
        P,
        None,
        BulkLimits {
            max_actions: 100,
            max_bytes: 1 << 20,
            max_retries: 0,
            base_backoff: std::time::Duration::ZERO,
        },
    );
    (backend, mock)
}

fn inputs(b: &ServerBackend) -> ServerInputs<'_> {
    ServerInputs {
        backend: b,
        endpoint: "https://user:secret@search.example:9200",
    }
}

#[test]
fn hundred_fifty_idle_sources_cost_at_most_thirty_requests() {
    let (backend, _) = setup(150);
    let before = backend.stats().requests;
    let r = collect(&inputs(&backend), &CollectOptions::default(), None).unwrap();
    let used = backend.stats().requests - before;
    assert!(used <= 30, "{used} requests");
    assert_eq!(r.diagnostics.request_count, Some(used));
    eprintln!("server 150-source full snapshot: {used} requests");
    assert_eq!(r.sources.len(), 150);
    let published = r.sources.iter().filter(|s| s.published.is_some()).count();
    assert_eq!(published, 147);
    assert_eq!(r.summary.files.discovered, Some(147 * 10));
    assert_eq!(r.summary.files.failed, Some(147));
    assert_eq!(r.summary.relationships, Some(147 * 5));
    assert_eq!(r.summary.entities, Some(147 * 7));
    assert_eq!(
        r.source("src_00049").unwrap().index_state,
        SourceIndexState::NotIndexed
    );
    assert_eq!(
        r.source("src_00000").unwrap().index_state,
        SourceIndexState::Retrying
    );
    assert_eq!(r.recent_errors.len(), DEFAULT_ERROR_LIMIT);
    assert!(r
        .recent_errors
        .windows(2)
        .all(|w| w[0].occurred_at >= w[1].occurred_at));
    assert_eq!(
        r.backend.cluster.as_ref().unwrap().health,
        Some(ClusterHealth::Green)
    );
    let endpoint = r.backend.endpoint.as_deref().unwrap();
    assert!(!endpoint.contains("secret"), "{endpoint}");
    assert_eq!(r.diagnostics.consistency, Consistency::Consistent);
    assert!(r.indexer.workers.is_empty());
}

#[test]
fn watch_refresh_reuses_aggregates_until_a_publication() {
    let (backend, mock) = setup(150);
    let mut session = ServerSession::default();
    let opts = CollectOptions::default();
    collect(&inputs(&backend), &opts, Some(&mut session)).unwrap();
    let before = backend.stats().requests;
    let r = collect(&inputs(&backend), &opts, Some(&mut session)).unwrap();
    let fast = backend.stats().requests - before;
    assert!(fast < 10, "{fast} requests per fast refresh");
    assert_eq!(fast, 3);
    assert_eq!(r.diagnostics.cached_sections.len(), 2);
    assert_eq!(r.summary.files.discovered, Some(147 * 10));
    // A publication: the next refresh reads the new build's counters.
    mock.world
        .lock()
        .unwrap()
        .builds
        .insert("src_00003".into(), Some("b-src_00003-2".into()));
    let r = collect(&inputs(&backend), &opts, Some(&mut session)).unwrap();
    assert!(r.diagnostics.cached_sections.is_empty());
    assert_eq!(
        r.source("src_00003")
            .unwrap()
            .published
            .as_ref()
            .unwrap()
            .files,
        12
    );
}

#[test]
fn a_publication_mid_snapshot_is_re_read_not_mixed() {
    let (backend, mock) = setup(150);
    mock.world.lock().unwrap().publish_mid_snapshot = Some("src_00007".into());
    let r = collect(&inputs(&backend), &CollectOptions::default(), None).unwrap();
    assert!(mock.published_once.load(Ordering::SeqCst));
    assert_eq!(r.diagnostics.consistency, Consistency::Retried);
    let s = r.source("src_00007").unwrap();
    assert_eq!(s.build.active_id.as_deref(), Some("b-src_00007-2"));
    assert_eq!(s.published.as_ref().unwrap().files, 12);
    assert!(r.diagnostics.request_count.unwrap() <= 30);
}

#[test]
fn ten_thousand_sources_page_without_truncation() {
    let (backend, _) = setup(10_050);
    let r = collect(&inputs(&backend), &CollectOptions::default(), None).unwrap();
    assert_eq!(r.sources.len(), 10_050);
    assert_eq!(r.summary.sources.registered, 10_050);
    let published = 10_050 - 10_050 / 50;
    assert_eq!(r.summary.files.discovered, Some(published as u64 * 10));
    // Bounded by pages and batches, not by sources.
    let used = r.diagnostics.request_count.unwrap();
    // 11 catalog pages + 11 re-check pages, 4 requests per 1000-source
    // batch (files, records, errors, graph generations), runtime and
    // cluster health.
    assert!(used <= 2 * 11 + 4 * 11 + 2, "{used}");
}

#[test]
fn a_failed_section_is_unknown_and_an_unreachable_cluster_is_an_error() {
    let (backend, mock) = setup(5);
    mock.world.lock().unwrap().fail_runtime = true;
    let r = collect(&inputs(&backend), &CollectOptions::default(), None).unwrap();
    assert!(r.diagnostics.partial);
    assert!(r.diagnostics.missing_sections[0].starts_with("runtime"));
    assert_eq!(r.health.state, HealthState::Unknown);
    mock.world.lock().unwrap().unreachable = true;
    let e = collect(&inputs(&backend), &CollectOptions::default(), None).unwrap_err();
    assert!(matches!(e, CollectError::Unavailable(_)), "{e:?}");
}
