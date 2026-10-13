//! Server status against real, disposable OpenSearch and Elasticsearch
//! clusters (`RAGMONK_TEST_{OPENSEARCH,ELASTICSEARCH}_URL`; with
//! `RAGMONK_REQUIRE_LIVE_SERVER=1` a missing URL fails instead of
//! skipping). Each scenario uses a fresh unique prefix and deletes it.
//!
//! * 150 registered sources: one snapshot costs ≤ 30 HTTP requests and
//!   every counter equals a raw count of the published build; a pending
//!   build is never counted.
//! * The 25 newest file errors are globally ordered by `last_error_at`.
//! * A pass heartbeating from host A is visible (source, stage) to a
//!   status reader on host B; a stale holder's write is fenced off; a run
//!   whose lease is released or expired is not reported as running.

use std::time::Duration;

use docmonk_backends::backend::{CatalogEntry, EdgeDoc, EntityDoc, FileDoc, LinkDoc};
use docmonk_backends::bulk::BulkLimits;
use docmonk_backends::engine::Engine;
use docmonk_backends::schema::{IndexKind, IndexSettings};
use docmonk_backends::status::RuntimeDoc;
use docmonk_backends::transport::{Auth, Counting, HttpTransport, Method};
use docmonk_backends::ServerBackend;
use docmonk_status::model::*;
use docmonk_status::server::{collect, ServerInputs, ServerSession};
use docmonk_status::CollectOptions;
use serde_json::json;

const SOURCES: usize = 150;

fn url(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(u) if !u.is_empty() => Some(u),
        _ => {
            assert!(
                std::env::var("RAGMONK_REQUIRE_LIVE_SERVER").as_deref() != Ok("1"),
                "{var} is required when RAGMONK_REQUIRE_LIVE_SERVER=1"
            );
            eprintln!("skipping: {var} not set");
            None
        }
    }
}

fn unique(tag: &str) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("rmstatus{tag}{}", n % 1_000_000_000)
}

/// A separate client (a separate "host") on the same prefix.
fn backend(base: &str, engine: Engine, prefix: &str) -> ServerBackend {
    ServerBackend::with_client(
        Counting::new(Box::new(
            HttpTransport::new(base, Auth::None, Duration::from_secs(60), true).unwrap(),
        )),
        engine,
        prefix,
        None,
        BulkLimits {
            max_actions: 2_000,
            max_bytes: 5_000_000,
            max_retries: 3,
            base_backoff: Duration::from_millis(50),
        },
    )
    .with_settings(IndexSettings {
        shards: 1,
        replicas: 0,
    })
}

fn drop_prefix(b: &ServerBackend) {
    for kind in IndexKind::ALL {
        let _ = b.delete_path(&format!("/{}", b.index(kind)));
    }
}

fn id(i: usize) -> String {
    format!("src_{i:03}")
}

/// 150 published sources: source i has 2 + i % 3 indexed files, one
/// failed file with a distinct error time, a retry file on every fifth
/// source, two entities, one edge and one link. Source 0 also has a
/// pending build that must stay invisible.
fn seed(b: &ServerBackend) {
    b.init().unwrap();
    let base_ms: i64 = 1_791_471_600_000;
    for i in 0..SOURCES {
        let src = id(i);
        b.register_source(&CatalogEntry {
            source_id: src.clone(),
            path: format!("/srv/{src}"),
            source_type: "local".into(),
            enabled: true,
            include_patterns: vec![],
            exclude_patterns: vec![],
            created_at: String::new(),
            updated_at: String::new(),
        })
        .unwrap();
    }
    for i in 0..SOURCES {
        let src = id(i);
        let build = format!("bld_{src}_1");
        b.begin_build(&src, &format!("/srv/{src}"), &build).unwrap();
        let mut w = b.build_writer(&src, &build);
        for k in 0..(2 + i % 3) {
            w.file(&FileDoc {
                file_id: format!("{src}_ok{k}"),
                rel_path: format!("ok{k}.py"),
                kind: "code".into(),
                status: Some("indexed".into()),
                ..Default::default()
            })
            .unwrap();
        }
        // Two sources share one error time: the tie breaks by source id.
        let at = base_ms + if i == 77 { 76 } else { i as i64 } * 60_000;
        w.file(&FileDoc {
            file_id: format!("{src}_bad"),
            rel_path: "bad.pdf".into(),
            kind: "document".into(),
            status: Some("failed".into()),
            attempt_count: Some(3),
            last_error: Some(format!("pdf broken in {src}")),
            last_error_at: Some(at.to_string()),
            ..Default::default()
        })
        .unwrap();
        if i % 5 == 0 {
            w.file(&FileDoc {
                file_id: format!("{src}_retry"),
                rel_path: "busy.py".into(),
                kind: "code".into(),
                status: Some("retry".into()),
                attempt_count: Some(1),
                last_error: Some("busy".into()),
                next_attempt_at: Some("2026-10-08T16:00:00+00:00".into()),
                ..Default::default()
            })
            .unwrap();
        }
        for n in ["a", "b"] {
            w.entity(&EntityDoc {
                entity_id: format!("{src}_e{n}"),
                file_id: format!("{src}_ok0"),
                rel_path: "ok0.py".into(),
                kind: "function".into(),
                name: n.into(),
                qualified_name: n.into(),
                language: "python".into(),
                start_line: 1,
                end_line: 2,
                ..Default::default()
            })
            .unwrap();
        }
        w.finish().unwrap();
        let graph = format!("{build}_g");
        b.publish_build(
            &src,
            &build,
            &json!({ "graph": { "state": "ready", "generation": graph, "base_build_id": build } }),
            true,
        )
        .unwrap();
        // Edges and links belong to the base's graph generation, written
        // after the base publication (which collects foreign records).
        let mut w = b.build_writer(&src, &graph);
        w.edge(&EdgeDoc {
            relationship_id: format!("{src}_r"),
            file_id: format!("{src}_ok0"),
            rel_path: "ok0.py".into(),
            relationship_type: "calls".into(),
            source_entity_id: format!("{src}_ea"),
            target_entity_id: Some(format!("{src}_eb")),
            resolver: "test".into(),
            confidence: "high".into(),
            ..Default::default()
        })
        .unwrap();
        w.link(&LinkDoc {
            relationship_id: format!("{src}_l"),
            relationship_type: "mentioned_in".into(),
            entity_id: format!("{src}_ea"),
            document_id: "d".into(),
            file_id: format!("{src}_ok0"),
            rel_path: "ok0.py".into(),
            resolver: "test".into(),
            confidence: "high".into(),
            ..Default::default()
        })
        .unwrap();
        w.finish().unwrap();
        b.set_online(&src, None, true, None).unwrap();
    }
    // A pending second build of source 0 with many records: invisible.
    let src = id(0);
    b.begin_build(&src, "/srv/src_000", "bld_src_000_2")
        .unwrap();
    let mut w = b.build_writer(&src, "bld_src_000_2");
    for k in 0..40 {
        w.file(&FileDoc {
            file_id: format!("pending{k}"),
            rel_path: format!("p{k}.py"),
            kind: "code".into(),
            status: Some("failed".into()),
            last_error: Some("pending".into()),
            last_error_at: Some((base_ms + 10_000_000).to_string()),
            ..Default::default()
        })
        .unwrap();
    }
    w.finish().unwrap();
    refresh(b, IndexKind::Files);
    refresh(b, IndexKind::Relationships);
}

fn refresh(b: &ServerBackend, kind: IndexKind) {
    let r = b
        .client()
        .send(Method::Post, &format!("/{}/_refresh", b.index(kind)), None)
        .unwrap();
    assert!(r.status < 300);
}

fn scenario(base: &str, engine: Engine) {
    let prefix = unique(if engine == Engine::OpenSearch {
        "os"
    } else {
        "es"
    });
    let writer = backend(base, engine, &prefix);
    seed(&writer);

    // ---- 150 idle sources, a different "host" reads --------------------
    let reader = backend(base, engine, &prefix);
    let inputs = ServerInputs {
        backend: &reader,
        endpoint: base,
    };
    let before = reader.stats().requests;
    let r = collect(&inputs, &CollectOptions::default(), None).unwrap();
    let used = reader.stats().requests - before;
    eprintln!("{engine:?}: 150-source snapshot used {used} HTTP requests");
    assert!(used <= 30, "{used} requests");
    assert_eq!(r.sources.len(), SOURCES);
    assert_eq!(r.mode, Mode::Server);
    assert!(r.backend.authoritative);
    for s in &r.sources {
        let b = s.build.active_id.as_deref().unwrap();
        let p = s.published.as_ref().unwrap();
        assert_eq!(
            p.files,
            reader.raw_count(IndexKind::Files, &s.source_id, b).unwrap()
        );
        assert_eq!(p.code_entities, 2);
        assert_eq!((p.relationships, p.links), (1, 1));
        assert_eq!(p.failed, 1);
    }
    let s0 = r.source(&id(0)).unwrap();
    assert_eq!(s0.build.active_id.as_deref(), Some("bld_src_000_1"));
    assert_eq!(s0.build.pending_id.as_deref(), Some("bld_src_000_2"));
    assert_eq!(
        s0.published.as_ref().unwrap().files,
        4,
        "pending files invisible"
    );
    assert_eq!(s0.index_state, SourceIndexState::Retrying);
    let want_files: u64 = (0..SOURCES)
        .map(|i| (2 + i % 3 + 1 + usize::from(i % 5 == 0)) as u64)
        .sum();
    assert_eq!(r.summary.files.discovered, Some(want_files));
    assert_eq!(r.summary.files.failed, Some(SOURCES as u64));
    assert_eq!(r.summary.files.retrying, Some(30));
    // Newest errors first, globally; the pending build's errors are absent.
    let errs: Vec<(&str, &str)> = r
        .recent_errors
        .iter()
        .map(|e| {
            (
                e.source_id.as_str(),
                e.occurred_at.as_deref().unwrap_or("-"),
            )
        })
        .collect();
    assert_eq!(errs.len(), DEFAULT_ERROR_LIMIT);
    assert_eq!(errs[0].0, id(149));
    assert!(r.recent_errors.iter().all(|e| e.message != "pending"));
    assert!(r
        .recent_errors
        .windows(2)
        .all(|w| w[0].occurred_at >= w[1].occurred_at));
    let src = |x: usize| r.source(&id(x)).unwrap();
    assert_eq!(src(77).last_error_at, src(76).last_error_at);
    let pos = |x: usize| r.recent_errors.iter().position(|e| e.source_id == id(x));
    if let (Some(a), Some(b)) = (pos(76), pos(77)) {
        assert!(a < b, "same time: ties break by source id");
    }
    assert!(r.backend.cluster.as_ref().unwrap().health.is_some());

    // ---- watch: a fast refresh reuses the published aggregates ---------
    let mut session = ServerSession::default();
    collect(&inputs, &CollectOptions::default(), Some(&mut session)).unwrap();
    let before = reader.stats().requests;
    let fast = collect(&inputs, &CollectOptions::default(), Some(&mut session)).unwrap();
    let used = reader.stats().requests - before;
    eprintln!("{engine:?}: fast watch refresh used {used} HTTP requests");
    assert!(used < 10);
    assert!(!fast.diagnostics.cached_sections.is_empty());

    // ---- a pass on host A is visible from host B ------------------------
    let lease = writer
        .acquire_lease(&id(5), "host-a:41", Duration::from_secs(60))
        .unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let doc = RuntimeDoc {
        source_id: id(5),
        run_id: "run-a".into(),
        host: "host-a".into(),
        owner: "host-a:41".into(),
        lease_token: lease.token,
        operation: Some("index".into()),
        stage: Some("processing".into()),
        active: true,
        scanned: 9,
        planned: Some(4),
        processed: 1,
        indexed: 1,
        heartbeat_at: Some(now.to_string()),
        expires_at: Some((now + 60_000).to_string()),
        ..RuntimeDoc::default()
    };
    assert!(writer.write_runtime(&doc).unwrap());
    // A stale holder (older token) cannot overwrite it.
    let stale = RuntimeDoc {
        lease_token: lease.token - 1,
        stage: Some("scanning".into()),
        ..doc.clone()
    };
    assert!(!writer.write_runtime(&stale).unwrap(), "fenced");
    refresh(&writer, IndexKind::Runtime);
    let r = collect(&inputs, &CollectOptions::default(), None).unwrap();
    let s5 = r.source(&id(5)).unwrap();
    assert_eq!(s5.index_state, SourceIndexState::Indexing);
    let live = s5.live.as_ref().unwrap();
    assert_eq!(
        (live.host.as_deref(), live.stage.as_deref(), live.liveness),
        (Some("host-a"), Some("processing"), Liveness::Live)
    );
    assert_eq!(live.percentage, Some(25.0));
    assert_eq!(live.pid, None);
    assert_eq!(r.indexer.scope, IndexerScope::Cluster);
    assert_eq!(r.indexer.active_source_count, 1);

    // ---- the lease goes away: not running ------------------------------
    writer.release_lease(&lease).unwrap();
    let r = collect(&inputs, &CollectOptions::default(), None).unwrap();
    let s5 = r.source(&id(5)).unwrap();
    assert_eq!(s5.live.as_ref().unwrap().liveness, Liveness::Expired);
    assert_ne!(s5.index_state, SourceIndexState::Indexing);
    assert_eq!(r.indexer.active_source_count, 0);
    assert!(r
        .problems
        .iter()
        .any(|p| p.code == ProblemCode::RunLeaseExpired));

    drop_prefix(&writer);
}

#[test]
fn opensearch_status() {
    if let Some(u) = url("RAGMONK_TEST_OPENSEARCH_URL") {
        scenario(&u, Engine::OpenSearch);
    }
}

#[test]
fn elasticsearch_status() {
    if let Some(u) = url("RAGMONK_TEST_ELASTICSEARCH_URL") {
        scenario(&u, Engine::Elasticsearch);
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() as f64 - 1.0) * p).round() as usize]
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// 150 sources with four passes heartbeating from two hosts: request
/// counts and latency percentiles of full snapshots and watch refreshes.
fn bench(base: &str, engine: Engine) -> serde_json::Value {
    let prefix = unique("bench");
    let writer = backend(base, engine, &prefix);
    seed(&writer);
    let now = chrono::Utc::now().timestamp_millis();
    for (n, host) in [(1, "host-a"), (2, "host-a"), (3, "host-b"), (4, "host-b")] {
        let owner = format!("{host}:{n}");
        let lease = writer
            .acquire_lease(&id(n), &owner, Duration::from_secs(600))
            .unwrap();
        writer
            .write_runtime(&RuntimeDoc {
                source_id: id(n),
                run_id: format!("run-{host}"),
                host: host.into(),
                owner,
                lease_token: lease.token,
                stage: Some("processing".into()),
                active: true,
                planned: Some(100),
                processed: 10,
                heartbeat_at: Some(now.to_string()),
                expires_at: Some((now + 600_000).to_string()),
                ..RuntimeDoc::default()
            })
            .unwrap();
    }
    refresh(&writer, IndexKind::Runtime);
    let reader = backend(base, engine, &prefix);
    let inputs = ServerInputs {
        backend: &reader,
        endpoint: base,
    };
    let opts = CollectOptions::default();
    let run = |session: Option<&mut ServerSession>| {
        let before = reader.stats().requests;
        let t = std::time::Instant::now();
        let r = collect(&inputs, &opts, session).unwrap();
        (
            t.elapsed().as_secs_f64() * 1000.0,
            reader.stats().requests - before,
            r,
        )
    };
    let (cold_ms, cold_requests, r) = run(None);
    assert_eq!(r.indexer.active_source_count, 4);
    let (mut full, mut full_requests) = (Vec::new(), 0);
    for _ in 0..30 {
        let (ms, n, _) = run(None);
        full.push(ms);
        full_requests = full_requests.max(n);
    }
    let mut session = ServerSession::default();
    run(Some(&mut session));
    let (mut fast, mut fast_requests) = (Vec::new(), 0);
    for _ in 0..30 {
        let (ms, n, _) = run(Some(&mut session));
        fast.push(ms);
        fast_requests = fast_requests.max(n);
    }
    full.sort_by(f64::total_cmp);
    fast.sort_by(f64::total_cmp);
    drop_prefix(&writer);
    assert!(full_requests <= 30 && fast_requests < 10);
    json!({
        "engine": format!("{engine:?}"),
        "sources": SOURCES,
        "active_passes": 4,
        "hosts": 2,
        "cold": {"requests": cold_requests, "ms": round1(cold_ms)},
        "full_snapshot": {"samples": 30, "max_requests": full_requests,
            "p50_ms": round1(percentile(&full, 0.5)), "p95_ms": round1(percentile(&full, 0.95))},
        "watch_refresh": {"samples": 30, "max_requests": fast_requests,
            "p50_ms": round1(percentile(&fast, 0.5)), "p95_ms": round1(percentile(&fast, 0.95))},
    })
}

#[test]
#[ignore = "benchmark: cargo test --release -p docmonk-status --test live_server -- --ignored"]
fn bench_status_150_sources() {
    for (var, engine) in [
        ("RAGMONK_TEST_OPENSEARCH_URL", Engine::OpenSearch),
        ("RAGMONK_TEST_ELASTICSEARCH_URL", Engine::Elasticsearch),
    ] {
        if let Some(u) = url(var) {
            println!("BENCH {}", bench(&u, engine));
        }
    }
}
