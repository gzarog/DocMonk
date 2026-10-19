//! URL-anchor retrieval through the service layer (P00/P02/P03/P07):
//! `evidence` and `explore` share the routed plan and the exact
//! occurrences, hard filters still bound them, `--explain` diagnostics are
//! opt-in, and the explore cache never serves a payload of an older build
//! or other settings.

// clean-slate-audit: allow-start (API URL paths with a version segment are user data)

use std::path::Path;

use docmonk_config::DocMonkConfig;
use docmonk_core::paths::Home;
use docmonk_retrieval::route::SearchFilters;
use docmonk_service::evidence::{evidence_value, EvidenceRequest};
use docmonk_service::query::{config_budget, explore_value_with, open_sources, ExploreRequest};
use serde_json::Value;

const Q01: &str = "Is the URL path /sb/v1/content/groups/image used, referenced, or related to \
                   any code or documentation in the indexed DocMonk project? Trace matching \
                   route/client/API references and relevant call paths.";

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let p = e.unwrap().path();
        let dest = to.join(p.file_name().unwrap());
        if p.is_dir() {
            copy_tree(&p, &dest);
        } else {
            std::fs::copy(&p, &dest).unwrap();
        }
    }
}

struct Env {
    _d: tempfile::TempDir,
    home: Home,
    a: String,
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
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/endpoints"),
        &ra,
    );
    // Source B only mentions the endpoint in a document.
    let rb = d.path().join("beta");
    std::fs::create_dir_all(rb.join("docs")).unwrap();
    std::fs::write(
        rb.join("docs/partner.md"),
        "# Partner API\n\nPartners poll /sb/v1/content/groups/image hourly.\n",
    )
    .unwrap();
    let mut c = docmonk_service::sources::catalog(&home).unwrap();
    let a = c.add(ra.to_str().unwrap(), vec![], vec![]).unwrap().id;
    c.add(rb.to_str().unwrap(), vec![], vec![]).unwrap();
    index(&home);
    Env {
        _d: d,
        home,
        a,
        root_a: ra,
    }
}

fn index(home: &Home) {
    let s = docmonk_service::indexing::selected_sources(home, None).unwrap();
    docmonk_service::indexing::index_sources(home, &s, "index", |_| {}).unwrap();
}

fn explore(env: &Env, cfg: &DocMonkConfig, q: &str, explain: bool) -> Value {
    let opened = open_sources(&env.home, None).unwrap();
    explore_value_with(
        &env.home,
        cfg,
        &opened,
        &ExploreRequest {
            query: q,
            budget: config_budget(cfg),
            explain,
        },
    )
    .unwrap()
}

fn classes(v: &Value) -> Vec<(String, String)> {
    v["exact_occurrences"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| {
            (
                o["evidence_class"].as_str().unwrap().to_owned(),
                o["path"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[test]
fn evidence_pins_exact_occurrences_within_the_filters() {
    let env = setup();
    let cfg = docmonk_service::load(&env.home).unwrap();
    let v = evidence_value(
        &env.home,
        &cfg,
        &EvidenceRequest {
            query: Q01,
            filters: SearchFilters::default(),
            limit: 10,
        },
    )
    .unwrap();
    assert_eq!(v["plan"]["intent"], "endpoint_reference");
    assert_eq!(
        v["plan"]["anchors"]["url_paths"][0]["path"],
        "/sb/v1/content/groups/image"
    );
    let tiers: Vec<&str> = v["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["tier"].as_str().unwrap())
        .collect();
    assert_eq!(tiers[0], "direct_exact_literal", "{tiers:?}");
    assert!(tiers.contains(&"statically_composed_route"), "{tiers:?}");
    assert!(v["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["tier"] == "direct_exact_literal")
        .all(|e| e["valid"] == true));
    assert!(classes(&v)
        .iter()
        .any(|(c, p)| c == "document_reference" && p.ends_with("partner.md")));
    // Source-scoped: B's document is neither an occurrence nor evidence.
    let scoped = evidence_value(
        &env.home,
        &cfg,
        &EvidenceRequest {
            query: Q01,
            filters: SearchFilters {
                source_ids: vec![env.a.clone()],
                ..Default::default()
            },
            limit: 10,
        },
    )
    .unwrap();
    assert!(!scoped.to_string().contains("partner.md"));
    assert!(!classes(&scoped).is_empty());
}

#[test]
fn explore_explain_is_opt_in_and_reconciles() {
    let env = setup();
    let cfg = docmonk_service::load(&env.home).unwrap();
    let plain = explore(&env, &cfg, Q01, false);
    assert!(plain.get("diagnostics").is_none());
    assert_eq!(plain["intent"], "endpoint_reference");
    // Paths are absolute, under the right source.
    assert!(classes(&plain)
        .iter()
        .any(|(c, p)| c == "direct_exact_literal" && Path::new(p).starts_with(&env.root_a)));
    let v = explore(&env, &cfg, Q01, true);
    let d = &v["diagnostics"];
    let service: Vec<&str> = d["service_stages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["stage"].as_str().unwrap())
        .collect();
    assert_eq!(
        service,
        ["plan", "graph_freshness", "explore", "absolutize"]
    );
    assert_eq!(d["backend"]["kind"], "sqlite");
    assert_eq!(d["builds"].as_array().unwrap().len(), 2);
    let total = d["total_ms"].as_f64().unwrap();
    let staged: f64 = d["service_stages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["duration_ms"].as_f64().unwrap())
        .sum();
    assert!(staged <= total + 1.0, "stages {staged} vs total {total}");
    assert_eq!(d["semantic"], "not_planned");
    // The explain payload never carries the query text in a log-like key.
    assert!(d.get("query").is_none());
}

#[test]
fn the_explore_cache_follows_builds_and_settings() {
    let env = setup();
    docmonk_service::query::enable_store_pool();
    let cfg = docmonk_service::load(&env.home).unwrap();
    let first = explore(&env, &cfg, "/sb/v1/content/groups/image", false);
    let hits = docmonk_service::query::explore_cache_hits();
    let again = explore(&env, &cfg, "/sb/v1/content/groups/image", false);
    assert_eq!(first, again);
    assert_eq!(docmonk_service::query::explore_cache_hits(), hits + 1);
    // `--explain` is never served from the cache.
    explore(&env, &cfg, "/sb/v1/content/groups/image", true);
    assert_eq!(docmonk_service::query::explore_cache_hits(), hits + 1);
    // A new literal and a new build: the cached payload is not served.
    std::fs::write(
        env.root_a.join("src/Clients/Extra.cs"),
        "namespace Shop.Clients { public class Extra { public void Go() { var u = \"/sb/v1/content/groups/image\"; } } }\n",
    )
    .unwrap();
    docmonk_service::query::release_pooled_stores();
    index(&env.home);
    let before_rebuild = docmonk_service::query::explore_cache_hits();
    let after = explore(&env, &cfg, "/sb/v1/content/groups/image", false);
    assert_eq!(docmonk_service::query::explore_cache_hits(), before_rebuild);
    assert!(
        classes(&after).iter().any(|(_, p)| p.ends_with("Extra.cs")),
        "{:?}",
        classes(&after)
    );
    // Other settings: another key (exact occurrences off).
    let mut off = cfg.clone();
    off.search.exact_occurrences.enabled = false;
    let v = explore(&env, &off, "/sb/v1/content/groups/image", false);
    assert!(v.get("exact_occurrences").is_none());
    docmonk_service::query::release_pooled_stores();
}
// clean-slate-audit: allow-end
