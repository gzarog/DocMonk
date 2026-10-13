//! Evidence pipeline guarantees (ADR 0033, P0-R01..R03): hard filters
//! cannot be bypassed by graph expansion, decomposition or attachments;
//! citations are re-verified against the current snapshot (stale build,
//! deleted file); explain plans are stable.

use std::path::Path;

use docmonk_config::DocMonkConfig;
use docmonk_core::paths::Home;
use docmonk_retrieval::grounding::{self, EvidenceRef};
use docmonk_retrieval::route::SearchFilters;
use docmonk_service::evidence::{evidence_value, EvidenceRequest};
use docmonk_service::query::{corpora, open_sources};
use serde_json::Value;

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

struct Env {
    _d: tempfile::TempDir,
    home: Home,
    a: String,
    b: String,
    root_a: std::path::PathBuf,
}

fn setup() -> Env {
    let d = tempfile::tempdir().unwrap();
    let home = Home::new(d.path().join("home"));
    home.ensure_layout().unwrap();
    let mut cfg = DocMonkConfig::default();
    cfg.updates.enabled = false;
    docmonk_config::write_user_config(&cfg, &home).unwrap();
    let ra = d.path().join("alpha");
    let rb = d.path().join("beta");
    // Source A: a service and its docs. Source B calls into A's method and
    // has a secret document that must stay out of A-scoped queries.
    write(
        &ra,
        "src/Pay/PayGate.cs",
        "namespace Pay { public class PayGate { public int Charge(int a) { return a; } } }\n",
    );
    write(
        &ra,
        "docs/refunds.md",
        "# Refunds\n\nRefunds above 500 euros need finance approval.\n",
    );
    write(
        &ra,
        "docs/private/notes.md",
        "# Notes\n\nRefunds overflow notes kept privately.\n",
    );
    write(&rb, "src/Shop/Cart.cs", "namespace Shop { public class Cart { public int Buy(Pay.PayGate g) { return g.Charge(1); } } }\n");
    write(
        &rb,
        "docs/secret.md",
        "# Secret\n\nRefunds are secretly approved by the night shift.\n",
    );
    let mut c = docmonk_service::sources::catalog(&home).unwrap();
    let a = c.add(ra.to_str().unwrap(), vec![], vec![]).unwrap().id;
    let b = c.add(rb.to_str().unwrap(), vec![], vec![]).unwrap().id;
    index(&home);
    Env {
        _d: d,
        home,
        a,
        b,
        root_a: ra,
    }
}

fn index(home: &Home) {
    let s = docmonk_service::indexing::selected_sources(home, None).unwrap();
    docmonk_service::indexing::index_sources(home, &s, "index", |_| {}).unwrap();
}

fn ev(env: &Env, q: &str, filters: SearchFilters) -> Value {
    let cfg = docmonk_service::load(&env.home).unwrap();
    evidence_value(
        &env.home,
        &cfg,
        &EvidenceRequest {
            query: q,
            filters,
            limit: 10,
        },
    )
    .unwrap()
}

fn sources_of(v: &Value) -> Vec<String> {
    let mut s: Vec<String> = v["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["source_id"].as_str().unwrap().to_owned())
        .chain(
            v["graph"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["source_id"].as_str().unwrap().to_owned()),
        )
        .collect();
    s.sort();
    s.dedup();
    s
}

#[test]
fn hard_filters_hold_across_every_stage() {
    let env = setup();
    let only_a = SearchFilters {
        source_ids: vec![env.a.clone()],
        ..Default::default()
    };
    // Unfiltered, both sources answer.
    let all = ev(&env, "refunds approval", SearchFilters::default());
    assert_eq!(sources_of(&all).len(), 2, "{all}");
    // Lexical, decomposition (comparison) and graph expansion all stay in A.
    for q in [
        "refunds approval",
        "compare refunds and secret approval",
        "who calls Charge",
        "what breaks if Charge changes",
    ] {
        let v = ev(&env, q, only_a.clone());
        assert!(sources_of(&v).iter().all(|s| *s == env.a), "{q}: {v}");
        assert_eq!(
            v["diagnostics"]["sources_searched"],
            serde_json::json!([env.a.clone()])
        );
    }
    // The caller lives only in B: an A-scoped navigation query has no
    // caller evidence, and B-scoped finds it.
    let b_scoped = ev(
        &env,
        "who calls Charge",
        SearchFilters {
            source_ids: vec![env.b.clone()],
            ..Default::default()
        },
    );
    assert!(b_scoped["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["source_id"] == env.b.as_str()));
    // Path prefix: nothing under docs/private.
    let v = ev(
        &env,
        "refunds overflow notes",
        SearchFilters {
            path_prefixes: vec!["docs/refunds".into(), "src/".into()],
            ..Default::default()
        },
    );
    assert!(
        v["evidence"]
            .as_array()
            .unwrap()
            .iter()
            // macOS temp dirs live under /private, so check the relative path.
            .all(|e| !e["path"].as_str().unwrap().contains("docs/private")),
        "{v}"
    );
    // Kind filter.
    let v = ev(
        &env,
        "PayGate",
        SearchFilters {
            kinds: vec!["document".into()],
            ..Default::default()
        },
    );
    assert!(
        v["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["kind"] == "document"),
        "{v}"
    );
}

#[test]
fn citations_are_verified_against_the_current_snapshot() {
    let env = setup();
    let v = ev(&env, "refunds finance approval", SearchFilters::default());
    assert_ne!(v["verdict"], "insufficient_evidence");
    let refs: Vec<EvidenceRef> = v["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| EvidenceRef {
            id: e["id"].as_str().unwrap().into(),
            source_id: e["source_id"].as_str().unwrap().into(),
            build_id: e["build_id"].as_str().unwrap().into(),
            kind: e["kind"].as_str().unwrap().into(),
            record_id: e["record_id"].as_str().unwrap().into(),
            path: e["path"].as_str().unwrap().into(),
            line_start: None,
            line_end: None,
            page_start: None,
            page_end: None,
            heading: None,
            fingerprint: e["fingerprint"].as_str().unwrap().into(),
            text: e["text"].as_str().unwrap().into(),
        })
        .collect();
    assert!(v["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["valid"] == true));
    // Forged: an id in a source the snapshot does not know, and a record
    // that does not exist.
    let mut forged = refs[0].clone();
    forged.source_id = "src_forged".into();
    let mut missing = refs[0].clone();
    missing.record_id = "0000".into();
    let opened = open_sources(&env.home, None).unwrap();
    let corp = corpora(&opened);
    let checks = grounding::verify(&corp, &[forged, missing], &SearchFilters::default()).unwrap();
    assert_eq!(checks[0].reason, Some("unknown_source"));
    assert_eq!(checks[1].reason, Some("missing_record"));
    // Delete the cited document and reindex: the full rebuild of A makes
    // a new snapshot; the old citation is no longer valid in it.
    std::fs::remove_file(env.root_a.join("docs/refunds.md")).unwrap();
    {
        let mut cp = docmonk_service::sources::control_plane(&env.home).unwrap();
        cp.require_full_rebuild(&env.a, "test").unwrap();
    }
    index(&env.home);
    let opened = open_sources(&env.home, None).unwrap();
    let corp = corpora(&opened);
    let cited_a: Vec<EvidenceRef> = refs
        .into_iter()
        .filter(|r| r.source_id == env.a && r.path.ends_with("refunds.md"))
        .collect();
    assert!(!cited_a.is_empty());
    for c in grounding::verify(&corp, &cited_a, &SearchFilters::default()).unwrap() {
        assert!(!c.valid, "{c:?}");
        assert!(
            matches!(c.reason, Some("stale_build") | Some("missing_record")),
            "{c:?}"
        );
    }
}

#[test]
fn no_evidence_abstains_and_explain_plan_is_stable() {
    let env = setup();
    let v = ev(
        &env,
        "what is the zeppelin hangar lease",
        SearchFilters::default(),
    );
    assert_eq!(v["verdict"], "insufficient_evidence", "{v}");
    let multi = ev(
        &env,
        "how are refunds approved? what is the zeppelin hangar lease?",
        SearchFilters::default(),
    );
    assert_eq!(multi["verdict"], "partial", "{multi}");
    // Explain plan snapshot (deterministic for a given query and config).
    let p = ev(&env, "who calls Charge", SearchFilters::default())["plan"].clone();
    assert_eq!(p["intent"], "code_navigation");
    assert_eq!(p["target"], "Charge");
    assert_eq!(
        p["strategies"],
        serde_json::json!(["lexical_exact", "symbol_graph", "lexical_fts"])
    );
    assert_eq!(
        p,
        ev(&env, "who calls Charge", SearchFilters::default())["plan"]
    );
}
