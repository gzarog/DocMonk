//! Evidence pipeline guarantees (ADR 0033, P0-R01..R03): hard filters
//! cannot be bypassed by graph expansion, decomposition or attachments;
//! citations are re-verified against the current snapshot (stale build,
//! deleted file, changed content, wrong location, another scope); explain
//! plans are stable.

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

/// The citations of an `evidence` payload, as the caller would keep them.
fn refs_of(v: &Value) -> Vec<EvidenceRef> {
    let s = |e: &Value, k: &str| e[k].as_str().map(str::to_owned);
    v["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| EvidenceRef {
            id: s(e, "id").unwrap(),
            source_id: s(e, "source_id").unwrap(),
            build_id: s(e, "build_id").unwrap(),
            kind: s(e, "kind").unwrap(),
            record_id: s(e, "record_id").unwrap(),
            path: s(e, "path").unwrap(),
            line_start: e["line_start"].as_i64(),
            line_end: e["line_end"].as_i64(),
            page_start: e["page_start"].as_i64(),
            page_end: e["page_end"].as_i64(),
            heading: s(e, "heading"),
            fingerprint: s(e, "fingerprint").unwrap(),
            content_fingerprint: s(e, "content_fingerprint"),
            language: s(e, "language"),
            text: s(e, "text").unwrap(),
        })
        .collect()
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
    let refs = refs_of(&v);
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
fn verification_rejects_changed_content_wrong_locations_and_other_scopes() {
    let env = setup();
    let v = ev(&env, "refunds finance approval", SearchFilters::default());
    let docs = refs_of(&v);
    let v = ev(&env, "Charge", SearchFilters::default());
    let code = refs_of(&v);
    let entity = code
        .iter()
        .find(|r| r.kind == "entity" && r.line_start.is_some())
        .expect("a located code citation")
        .clone();
    let doc = docs
        .iter()
        .find(|r| r.kind == "document" && r.source_id == env.a)
        .expect("a document citation")
        .clone();
    // Every citation carries the fingerprint of its record's stored content.
    assert!(entity.content_fingerprint.is_some() && doc.content_fingerprint.is_some());
    assert_eq!(v["provenance"]["invalid"], 0, "{}", v["provenance"]);
    assert_eq!(
        v["provenance"]["checked"],
        v["evidence"].as_array().unwrap().len()
    );

    let mut changed = doc.clone();
    changed.content_fingerprint = Some("0000000000000000".into());
    let mut misplaced = entity.clone();
    misplaced.line_start = Some(900);
    misplaced.line_end = Some(905);
    let mut stale = entity.clone();
    stale.build_id = "build_elsewhere".into();
    let mut gone = doc.clone();
    gone.record_id = "0000".into();
    let opened = open_sources(&env.home, None).unwrap();
    let corp = corpora(&opened);
    let checks = grounding::verify(
        &corp,
        &[entity.clone(), doc.clone(), changed, misplaced, stale, gone],
        &SearchFilters::default(),
    )
    .unwrap();
    let reasons: Vec<Option<&str>> = checks.iter().map(|c| c.reason).collect();
    assert_eq!(
        reasons,
        [
            None,
            None,
            Some("content_mismatch"),
            Some("wrong_location"),
            Some("stale_build"),
            Some("missing_record"),
        ]
    );
    // Outside the request's scope: a citation from A is not valid for a
    // B-scoped request, nor for a kind it excludes.
    let only_b = SearchFilters {
        source_ids: vec![env.b.clone()],
        ..Default::default()
    };
    let only_docs = SearchFilters {
        kinds: vec!["document".into()],
        ..Default::default()
    };
    assert_eq!(
        grounding::verify(&corp, std::slice::from_ref(&doc), &only_b).unwrap()[0].reason,
        Some("filtered")
    );
    assert_eq!(
        grounding::verify(&corp, std::slice::from_ref(&entity), &only_docs).unwrap()[0].reason,
        Some("filtered")
    );
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
