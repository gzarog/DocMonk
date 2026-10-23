//! Citation hydration on a server (ADR 0041): the canonical content of an
//! answer's citations is read once, batched per corpus, and reused by the
//! verification, instead of one entity and one passage request per
//! citation, twice. Runs on the engines `DOCMONK_TEST_OPENSEARCH_URL` /
//! `DOCMONK_TEST_ELASTICSEARCH_URL` point at (skipped otherwise).

use std::path::Path;

use docmonk_config::DocMonkConfig;
use docmonk_core::paths::Home;
use docmonk_retrieval::grounding;
use docmonk_retrieval::route::SearchFilters;
use docmonk_service::evidence::{evidence_value, EvidenceRequest};
use docmonk_service::query::{corpora, open_sources};

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

fn run(engine: &str, url: &str) {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::new(dir.path().join("home"));
    home.ensure_layout().unwrap();
    let prefix = format!(
        "hydrate{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            % 1_000_000_000
    );
    let mut cfg = DocMonkConfig::default();
    cfg.updates.enabled = false;
    cfg.storage.mode = "server".into();
    cfg.storage.server.engine = match engine {
        "opensearch" => docmonk_config::model::StorageEngine::OpenSearch,
        _ => docmonk_config::model::StorageEngine::Elasticsearch,
    };
    cfg.storage.server.url = url.into();
    cfg.storage.server.index_prefix = prefix.clone();
    docmonk_config::write_user_config(&cfg, &home).unwrap();
    let root = dir.path().join("src");
    // Twelve functions that all compute a refund: a code question cites
    // many entities.
    for i in 0..12 {
        write(
            &root,
            &format!("billing/refund_{i}.py"),
            &format!(
                "def refund_amount_{i}(order):\n    \"\"\"Refund amount for order {i}.\"\"\"\n    \
                 fee = order.total * 0.0{i}\n    return order.total - fee\n"
            ),
        );
    }
    docmonk_service::sources::catalog(&home)
        .unwrap()
        .add(root.to_str().unwrap(), vec![], vec![])
        .unwrap();
    let s = docmonk_service::indexing::selected_sources(&home, None).unwrap();
    docmonk_service::indexing::index_sources(&home, &s, "index", |_| {}).unwrap();

    let cfg = docmonk_service::load(&home).unwrap();
    let v = evidence_value(
        &home,
        &cfg,
        &EvidenceRequest {
            query: "how is the refund amount computed for an order",
            filters: SearchFilters::default(),
            limit: 10,
        },
    )
    .unwrap();
    let cited: Vec<&serde_json::Value> = v["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "entity")
        .collect();
    assert!(cited.len() >= 5, "{engine}: {v}");
    assert_eq!(v["diagnostics"]["backend"]["kind"], engine);
    assert!(v["provenance"]["invalid"] == 0, "{}", v["provenance"]);

    // The same citations, hydrated per citation and twice (the former
    // path) and batched once (the current one).
    let opened = open_sources(&home, None).unwrap();
    let corp = corpora(&opened);
    let server = opened[0].store.server().unwrap();
    let refs: Vec<grounding::EvidenceRef> = cited
        .iter()
        .map(|e| grounding::EvidenceRef {
            id: e["id"].as_str().unwrap().into(),
            source_id: e["source_id"].as_str().unwrap().into(),
            build_id: e["build_id"].as_str().unwrap().into(),
            kind: "entity".into(),
            record_id: e["record_id"].as_str().unwrap().into(),
            path: e["path"].as_str().unwrap().into(),
            line_start: e["line_start"].as_i64(),
            line_end: e["line_end"].as_i64(),
            page_start: None,
            page_end: None,
            heading: None,
            fingerprint: e["fingerprint"].as_str().unwrap().into(),
            content_fingerprint: e["content_fingerprint"].as_str().map(str::to_owned),
            language: None,
            text: e["text"].as_str().unwrap().into(),
        })
        .collect();
    let requests = || server.backend_stats().requests;
    let t0 = requests();
    for _ in 0..2 {
        for r in &refs {
            let lines = r.line_start.map(|s| (s, r.line_end.unwrap_or(s)));
            grounding::canonical_content(&corp[0], &r.kind, &r.record_id, lines).unwrap();
        }
    }
    let per_citation = requests() - t0;
    let t1 = requests();
    let contents = grounding::canonical_contents(&corp, &refs).unwrap();
    let checks =
        grounding::verify_with(&corp, &refs, &SearchFilters::default(), &contents).unwrap();
    let batched = requests() - t1;
    assert!(checks.iter().all(|c| c.valid), "{checks:?}");
    eprintln!(
        "{engine}: {} entity citations: {per_citation} requests per citation, {batched} batched",
        refs.len()
    );
    assert!(
        batched * 4 <= per_citation,
        "{engine}: {batched} vs {per_citation}"
    );

    let c = reqwest::blocking::Client::new();
    for kind in docmonk_backends::schema::IndexKind::ALL {
        let _ = c
            .delete(format!(
                "{url}/{}",
                docmonk_backends::schema::index_name(&prefix, kind)
            ))
            .send();
    }
}

#[test]
fn citations_are_hydrated_once_and_batched_on_servers() {
    for (engine, var) in [
        ("opensearch", "DOCMONK_TEST_OPENSEARCH_URL"),
        ("elasticsearch", "DOCMONK_TEST_ELASTICSEARCH_URL"),
    ] {
        match std::env::var(var).ok().filter(|u| !u.is_empty()) {
            Some(url) => run(engine, &url),
            None => eprintln!("skipping {engine}: {var} not set"),
        }
    }
}
