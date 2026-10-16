//! Server mode end to end against real, disposable OpenSearch and
//! Elasticsearch clusters (ADR 0033, P0-S01..S04).
//!
//! Set `DOCMONK_TEST_OPENSEARCH_URL` / `DOCMONK_TEST_ELASTICSEARCH_URL`;
//! with `DOCMONK_REQUIRE_LIVE_SERVER=1` a missing URL fails instead of
//! skipping. Every scenario uses a fresh home and a unique index prefix and
//! deletes its indexes afterwards. Never point these at a real deployment.

use std::path::{Path, PathBuf};
use std::time::Duration;

use docmonk_backends::schema::IndexKind;
use docmonk_config::DocMonkConfig;
use docmonk_core::errors::ErrorKind;
use docmonk_core::paths::Home;
use docmonk_service::indexing::{index_sources_with, selected_sources, RunOptions, SourceEvent};
use docmonk_service::query::{self, open_sources};
use docmonk_service::sources::{catalog, docs_rows};
use serde_json::Value;

fn url(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(u) if !u.is_empty() => Some(u),
        _ => {
            assert!(
                std::env::var("DOCMONK_REQUIRE_LIVE_SERVER").as_deref() != Ok("1"),
                "{var} is required when DOCMONK_REQUIRE_LIVE_SERVER=1"
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
    format!("rmsvc{tag}{}", n % 1_000_000_000)
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let p = e.path();
        if p.is_dir() {
            copy_dir(&p, &to.join(e.file_name()));
        } else {
            std::fs::copy(&p, to.join(e.file_name())).unwrap();
        }
    }
}

struct Env {
    _dir: tempfile::TempDir,
    home: Home,
    code: PathBuf,
    docs: PathBuf,
    base: String,
    prefix: String,
}

impl Drop for Env {
    fn drop(&mut self) {
        let c = reqwest::blocking::Client::new();
        let _ = c.delete(format!("{}/{}-*", self.base, self.prefix)).send();
    }
}

fn setup(base: &str, engine: &str, tag: &str) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::new(dir.path().join("home"));
    home.ensure_layout().unwrap();
    let prefix = unique(tag);
    let mut cfg = DocMonkConfig::default();
    cfg.storage.mode = "server".into();
    cfg.storage.server.engine = match engine {
        "opensearch" => docmonk_config::model::StorageEngine::OpenSearch,
        _ => docmonk_config::model::StorageEngine::Elasticsearch,
    };
    cfg.storage.server.url = base.into();
    cfg.storage.server.index_prefix = prefix.clone();
    cfg.storage.server.gc_grace_seconds = 0.0;
    cfg.storage.server.lease_seconds = 30.0;
    cfg.updates.enabled = false;
    cfg.indexing.max_parallel_sources = 2;
    docmonk_config::write_user_config(&cfg, &home).unwrap();
    let code = dir.path().join("src-code");
    copy_dir(&fixtures().join("search_quality/project"), &code);
    let docs = dir.path().join("src-docs");
    std::fs::create_dir_all(&docs).unwrap();
    for f in [
        "sample.pdf",
        "email_with_attachments.eml",
        "handbook.md",
        "simple.csv",
    ] {
        std::fs::copy(fixtures().join("documents").join(f), docs.join(f)).unwrap();
    }
    Env {
        _dir: dir,
        home,
        code,
        docs,
        base: base.into(),
        prefix,
    }
}

/// `(completed, failed/blocked)` source ids and every completed result.
fn index(env: &Env, force_full: bool) -> (Vec<Value>, Vec<String>) {
    let sources = selected_sources(&env.home, None).unwrap();
    let mut done = Vec::new();
    let mut bad = Vec::new();
    let summary = index_sources_with(
        &env.home,
        &sources,
        "index",
        &RunOptions {
            force_full,
            ..RunOptions::default()
        },
        |e| match e {
            SourceEvent::Completed { source, run } => done.push(serde_json::json!({
                "id": source.id,
                "published": run.published,
                "offline": run.offline,
                "build_id": run.build_id,
                "publish": run.server_publish,
                "indexed": run.indexed,
            })),
            SourceEvent::Failed { source, error } | SourceEvent::Blocked { source, error } => {
                bad.push(format!("{}: {}", source.id, error.message()))
            }
            SourceEvent::Relationships { source, outcome } => {
                if outcome.is_failure() {
                    bad.push(format!(
                        "{}: relationships {}: {}",
                        source.id,
                        outcome.state(),
                        outcome.error().unwrap_or_default()
                    ))
                }
            }
            SourceEvent::Started { .. } | SourceEvent::Dependencies { .. } => {}
        },
    )
    .unwrap();
    assert!(summary.max_parallel_sources >= 1);
    (done, bad)
}

fn server(env: &Env) -> std::sync::Arc<docmonk_backends::ServerBackend> {
    match docmonk_service::backend::open(&env.home).unwrap() {
        docmonk_service::backend::Backend::Server(s) => s,
        _ => panic!("expected server mode"),
    }
}

fn scenario(base: &str, engine: &str) {
    let env = setup(base, engine, "e2e");
    // --- catalog (S01) ----------------------------------------------------
    let mut c = catalog(&env.home).unwrap();
    assert!(c.is_server());
    let a = c.add(env.code.to_str().unwrap(), vec![], vec![]).unwrap();
    let b = c.add(env.docs.to_str().unwrap(), vec![], vec![]).unwrap();
    // Idempotent by path.
    assert_eq!(
        c.add(env.code.to_str().unwrap(), vec![], vec![])
            .unwrap()
            .id,
        a.id
    );
    // Authoritative: a fresh process (new catalog handle) sees them.
    let listed: Vec<String> = catalog(&env.home)
        .unwrap()
        .list(false)
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(listed.len(), 2);
    assert!(listed.contains(&a.id) && listed.contains(&b.id));
    // No local registry or knowledge was created.
    assert!(!env.home.root().join("state/control.db").exists());

    // --- cold index (S02, I01) ------------------------------------------
    let (done, bad) = index(&env, false);
    assert!(bad.is_empty(), "{bad:?}");
    assert_eq!(done.len(), 2);
    for d in &done {
        assert_eq!(d["published"], true, "{d}");
    }
    let projects = env.home.root().join("projects");
    assert!(
        !projects.exists() || std::fs::read_dir(&projects).unwrap().next().is_none(),
        "server mode must not write local project knowledge"
    );
    let srv = server(&env);
    let active = srv.active_builds().unwrap();
    assert_eq!(active.len(), 2);
    for kind in [
        IndexKind::Files,
        IndexKind::Code,
        IndexKind::Documents,
        IndexKind::Chunks,
    ] {
        assert!(srv.count(kind, None).unwrap() > 0, "{kind:?} is empty");
    }

    // --- queries (S03) -----------------------------------------------------
    let opened = open_sources(&env.home, None).unwrap();
    assert_eq!(opened.len(), 2);
    assert!(opened
        .iter()
        .all(|o| o.store.read().backend_kind() == engine));
    let sym = query::symbol_value(&opened, "SettlementService").unwrap();
    let m = sym["matches"].as_array().unwrap();
    assert!(!m.is_empty(), "{sym}");
    assert_eq!(m[0]["source_id"], a.id.as_str());
    let calls = query::calls_value(
        &opened,
        "services.settlement_service.SettlementService.process",
        docmonk_retrieval::graph::Direction::Outgoing,
        2,
        50,
    )
    .unwrap();
    assert!(!calls["matches"].as_array().unwrap().is_empty(), "{calls}");
    // process() calls retry_settlement(): an edge with line provenance.
    let edges = calls["edges"].as_array().unwrap();
    assert!(
        edges.iter().any(|e| !e["target_entity_id"].is_null()
            && e["evidence"]
                .as_str()
                .is_some_and(|t| t.contains("retry_settlement"))
            && e["source_location"]
                .as_str()
                .is_some_and(|l| l.contains("settlement_service.py:"))),
        "{calls}"
    );
    let lex = query::lexical_value(
        &docmonk_service::load(&env.home).unwrap(),
        &opened,
        "settlement",
        20,
    )
    .unwrap();
    let results = lex["results"].as_array().unwrap();
    assert!(!results.is_empty(), "{lex}");
    // Provenance: absolute paths under the indexed root.
    assert!(results.iter().all(|r| r["path"]
        .as_str()
        .unwrap()
        .starts_with(env.code.to_str().unwrap())
        || r["path"]
            .as_str()
            .unwrap()
            .starts_with(env.docs.to_str().unwrap())));
    let docs = docs_rows(&env.home, Some(&b.id)).unwrap();
    assert!(
        docs.iter()
            .any(|d| d["format"] == "eml" && !d["attachments"].as_array().unwrap().is_empty()),
        "{docs:?}"
    );
    assert!(docs.iter().any(|d| d["format"] == "pdf"));
    // Single-source filter excludes the other source.
    let only = open_sources(&env.home, Some(&b.id)).unwrap();
    let sym_b = query::symbol_value(&only, "SettlementService").unwrap();
    assert!(sym_b["matches"].as_array().unwrap().is_empty());

    // --- evidence pipeline on the server (R01-R03) -----------------------
    let cfg = docmonk_service::load(&env.home).unwrap();
    let ev = docmonk_service::evidence::evidence_value(
        &env.home,
        &cfg,
        &docmonk_service::evidence::EvidenceRequest {
            query: "SettlementService",
            filters: docmonk_retrieval::route::SearchFilters {
                source_ids: vec![a.id.clone()],
                ..Default::default()
            },
            limit: 5,
        },
    )
    .unwrap();
    assert_eq!(ev["plan"]["intent"], "exact_symbol", "{ev}");
    let items = ev["evidence"].as_array().unwrap();
    assert!(!items.is_empty(), "{ev}");
    assert!(
        items
            .iter()
            .all(|e| e["valid"] == true && e["source_id"] == a.id.as_str()),
        "{ev}"
    );
    let none = docmonk_service::evidence::evidence_value(
        &env.home,
        &cfg,
        &docmonk_service::evidence::EvidenceRequest {
            query: "what is the zeppelin hangar lease",
            filters: Default::default(),
            limit: 5,
        },
    )
    .unwrap();
    assert_eq!(none["verdict"], "insufficient_evidence", "{none}");

    // --- status reconciles with published records (S03) -----------------
    let st = docmonk_service::status::report(&env.home).unwrap();
    assert_eq!(st.backend.kind.as_str(), engine);
    assert!(st.backend.authoritative);
    assert_eq!(
        st.summary.files.discovered,
        Some(srv.count(IndexKind::Files, None).unwrap())
    );
    assert_eq!(
        st.summary.entities,
        Some(srv.count(IndexKind::Code, None).unwrap())
    );
    // The pass left a finished heartbeat document on the server.
    let row = st.source(&a.id).unwrap();
    let live = row.live.as_ref().expect("runtime document of the pass");
    assert_eq!(live.liveness, docmonk_status::model::Liveness::Finished);
    assert_eq!(live.outcome.as_deref(), Some("completed"));
    assert!(live.host.is_some() && live.pid.is_none());
    assert_eq!(
        row.index_state,
        docmonk_status::model::SourceIndexState::Completed
    );

    // --- warm pass: no new generation -----------------------------------
    let (done, bad) = index(&env, false);
    assert!(bad.is_empty(), "{bad:?}");
    assert!(done.iter().all(|d| d["published"] == false), "{done:?}");
    assert_eq!(srv.active_builds().unwrap(), active);

    // --- edit one file: copy-forward + rewrite ------------------------------
    std::fs::write(
        env.code.join("docs/settlement_guide.md"),
        "# Settlement guide\n\nThe zanzibarquartz reconciliation runs nightly.\n",
    )
    .unwrap();
    let (done, bad) = index(&env, false);
    assert!(bad.is_empty(), "{bad:?}");
    let da = done.iter().find(|d| d["id"] == a.id.as_str()).unwrap();
    assert_eq!(da["published"], true, "{da}");
    let p = &da["publish"];
    assert!(p["files_copied"].as_u64().unwrap() >= 4, "{p}");
    assert_eq!(p["files_written"].as_u64().unwrap(), 1, "{p}");
    let new_active = srv.active_builds().unwrap();
    assert_ne!(new_active[&a.id], active[&a.id]);
    assert_eq!(new_active[&b.id], active[&b.id], "other source untouched");
    let opened = open_sources(&env.home, None).unwrap();
    let hit = query::lexical_value(
        &docmonk_service::load(&env.home).unwrap(),
        &opened,
        "zanzibarquartz",
        5,
    )
    .unwrap();
    assert_eq!(hit["results"].as_array().unwrap().len(), 1, "{hit}");
    // Grace 0: the replaced build's own record of the edited file is gone;
    // only records shared with the new build (copied forward) still carry
    // its id until the next copy-forward prunes it.
    assert_eq!(
        srv.raw_count(IndexKind::Files, &a.id, &active[&a.id])
            .unwrap(),
        p["files_copied"].as_u64().unwrap()
    );

    // --- staging cache loss: rebuild locally, still copy forward --------------
    std::fs::remove_dir_all(env.home.cache_dir().join("server-staging")).unwrap();
    std::fs::write(
        env.code.join("docs/health_notes.md"),
        "# Health\n\nprobe xylophonequill\n",
    )
    .unwrap();
    let (done, bad) = index(&env, false);
    assert!(bad.is_empty(), "{bad:?}");
    let da = done.iter().find(|d| d["id"] == a.id.as_str()).unwrap();
    assert!(da["publish"]["files_copied"].as_u64().unwrap() >= 4, "{da}");
    assert_eq!(da["publish"]["files_written"].as_u64().unwrap(), 1, "{da}");
    let db = done.iter().find(|d| d["id"] == b.id.as_str()).unwrap();
    assert_eq!(
        db["published"], false,
        "reconverted but unchanged: no new generation {db}"
    );

    // --- offline source keeps its published knowledge ----------------------
    let before = srv.active_builds().unwrap();
    let moved = env.docs.with_extension("away");
    std::fs::rename(&env.docs, &moved).unwrap();
    let (done, _) = index(&env, false);
    let db = done.iter().find(|d| d["id"] == b.id.as_str()).unwrap();
    assert!(!db["offline"].is_null(), "{db}");
    assert_eq!(srv.active_builds().unwrap()[&b.id], before[&b.id]);
    assert!(srv.count(IndexKind::Documents, Some(&b.id)).unwrap() > 0);
    std::fs::rename(&moved, &env.docs).unwrap();

    // --- fencing: another host's live lease blocks this source -------------
    let foreign = srv
        .acquire_lease(&a.id, "otherhost:1", Duration::from_secs(60))
        .unwrap();
    let (done, bad) = index(&env, true);
    assert_eq!(bad.len(), 1, "{bad:?}");
    assert!(
        bad[0].contains(&a.id) && bad[0].contains("lease"),
        "{bad:?}"
    );
    assert!(done.iter().all(|d| d["id"] != a.id.as_str()));
    assert_eq!(srv.active_builds().unwrap()[&a.id], before[&a.id]);
    srv.release_lease(&foreign).unwrap();

    // --- manual links ----------------------------------------------------------
    let added = query::link_add(
        &env.home,
        "SettlementService",
        "settlement_guide.md",
        None,
        Some(&a.id),
    )
    .unwrap();
    let link_id = match added {
        query::LinkAdded::Added { link_id, .. } => link_id,
        query::LinkAdded::Exists => panic!("new link expected"),
    };
    let rows = query::link_list(&env.home, Some("SettlementService"), None, None).unwrap();
    assert!(
        rows.iter()
            .any(|r| r["link_id"] == link_id.as_str() && r["resolver"] == "user"),
        "{rows:?}"
    );
    query::link_remove(&env.home, &link_id, None).unwrap();
    let rows = query::link_list(&env.home, Some("SettlementService"), None, None).unwrap();
    assert!(!rows.iter().any(|r| r["link_id"] == link_id.as_str()));

    // --- remove ------------------------------------------------------------
    docmonk_service::sources::remove_source(&env.home, &b.id).unwrap();
    assert_eq!(srv.count(IndexKind::Documents, Some(&b.id)).unwrap(), 0);
    assert_eq!(catalog(&env.home).unwrap().list(false).unwrap().len(), 1);
}

/// A configured but unreachable server never falls back to local data,
/// even with a populated local home.
fn no_local_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::new(dir.path().join("home"));
    home.ensure_layout().unwrap();
    // Populate a local index first (local mode).
    let src = dir.path().join("src");
    copy_dir(&fixtures().join("search_quality/project"), &src);
    let mut cfg = DocMonkConfig::default();
    cfg.updates.enabled = false;
    docmonk_config::write_user_config(&cfg, &home).unwrap();
    catalog(&home)
        .unwrap()
        .add(src.to_str().unwrap(), vec![], vec![])
        .unwrap();
    let sources = selected_sources(&home, None).unwrap();
    docmonk_service::indexing::index_sources(&home, &sources, "index", |_| {}).unwrap();
    assert!(!open_sources(&home, None).unwrap().is_empty());
    // Switch to an unreachable server.
    cfg.storage.mode = "server".into();
    cfg.storage.server.url = "http://127.0.0.1:9".into();
    cfg.storage.server.request_timeout_seconds = 2.0;
    docmonk_config::write_user_config(&cfg, &home).unwrap();
    for e in [
        open_sources(&home, None).err().unwrap(),
        catalog(&home).err().unwrap(),
        docmonk_service::status::report(&home).err().unwrap(),
        selected_sources(&home, None).err().unwrap(),
        docmonk_service::sources::control_plane(&home)
            .err()
            .unwrap(),
    ] {
        assert!(
            matches!(
                e.kind(),
                ErrorKind::SourceUnavailable | ErrorKind::LocalStorageModeRequired
            ),
            "{e:?}"
        );
    }
}

#[test]
fn unavailable_server_never_reads_local_knowledge() {
    no_local_fallback();
}

#[test]
fn opensearch_end_to_end() {
    if let Some(u) = url("DOCMONK_TEST_OPENSEARCH_URL") {
        scenario(&u, "opensearch");
    }
}

#[test]
fn elasticsearch_end_to_end() {
    if let Some(u) = url("DOCMONK_TEST_ELASTICSEARCH_URL") {
        scenario(&u, "elasticsearch");
    }
}

/// F04 (audit PH02): a consumer resolves a producer published by another
/// host, without any staging copy of the producer on its own host.
fn cross_host_dependencies(base: &str, engine: &str) {
    use docmonk_service::dependencies::DependencyOutcome;
    let host1 = setup(base, engine, "xh");
    // A second host: its own home, same server and prefix.
    let home2 = Home::new(host1._dir.path().join("home2"));
    home2.ensure_layout().unwrap();
    let cfg = docmonk_service::load(&host1.home).unwrap();
    docmonk_config::write_user_config(&cfg, &home2).unwrap();

    let root = host1._dir.path();
    let write = |rel: &str, body: &str| {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    };
    write(
        "producer/Lib/Shared.cs",
        "namespace Lib {\n  public class Shared {\n    public static int Compute() { return 1; }\n  }\n}\n",
    );
    write("producer/Lib/Lib.csproj", "<Project></Project>\n");
    write(
        "consumer/App/Program.cs",
        "namespace App {\n  public class Program {\n    public static int Main() { return Lib.Shared.Compute(); }\n  }\n}\n",
    );
    write(
        "consumer/App/App.csproj",
        "<Project><ItemGroup>\n  <ProjectReference Include=\"../../producer/Lib/Lib.csproj\" />\n</ItemGroup></Project>\n",
    );
    let mut c = catalog(&host1.home).unwrap();
    let producer = c
        .add(root.join("producer").to_str().unwrap(), vec![], vec![])
        .unwrap();
    let consumer = c
        .add(root.join("consumer").to_str().unwrap(), vec![], vec![])
        .unwrap();

    // Host 1 indexes the producer only.
    docmonk_service::indexing::index_sources(
        &host1.home,
        std::slice::from_ref(&producer),
        "index",
        |_| {},
    )
    .unwrap();
    // Host 2 indexes the consumer only: it has no staging copy of the
    // producer.
    let mut outcome = None;
    docmonk_service::indexing::index_sources(
        &home2,
        std::slice::from_ref(&consumer),
        "index",
        |e| {
            if let SourceEvent::Dependencies { source, outcome: o } = e {
                if source.id == consumer.id {
                    outcome = Some(o.clone());
                }
            }
        },
    )
    .unwrap();
    let outcome = outcome.expect("consumer visited");
    assert!(
        matches!(outcome, DependencyOutcome::Updated { bound: 1, .. }),
        "{outcome:?}"
    );
    let m: docmonk_storage::graph::ExternalManifest = serde_json::from_value(
        server(&host1)
            .graph_external(&consumer.id)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(m.state, "current");
    assert!(m.revision > 0);
    let binding = &m.bindings[0];
    assert_eq!(binding.producer.as_deref(), Some(producer.id.as_str()));
    assert!(binding.target_entity_id.is_some() && !binding.ambiguous);
}

#[test]
fn opensearch_cross_host_dependencies() {
    if let Some(u) = url("DOCMONK_TEST_OPENSEARCH_URL") {
        cross_host_dependencies(&u, "opensearch");
    }
}

#[test]
fn elasticsearch_cross_host_dependencies() {
    if let Some(u) = url("DOCMONK_TEST_ELASTICSEARCH_URL") {
        cross_host_dependencies(&u, "elasticsearch");
    }
}
