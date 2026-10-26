//! Link and call-edge precision (P0.3).
//!
//! The synthetic corpus of the benchmarks (`docmonk_service::bench::corpus`)
//! knows exactly what it planted, so every cross-link and every resolved
//! call edge is labelled by the generator, not sampled:
//!
//! * Each `## Orders {i}` section of a design note mentions
//!   `Orders{i}Controller`, `OrderService{i}` (also as `OrderService{i}.cs`,
//!   `Shop.Services.OrderService{i}.Save` and `OrderService{i}.Find`) and
//!   the route `GET api/orders{i}/{id}`. A link from that section is
//!   correct when its entity is defined in `Orders{i}Controller.cs` or
//!   `OrderService{i}.cs`; anything else (`Find` of another service, a
//!   link from the title) is wrong. Recall counts the five planted
//!   entities per section (both classes, `Save`, `Find`, `Get`).
//! * Call edges: `Orders{i}Controller.Get` calls `Find` on its service and
//!   `Mapper{i+1}.ToDto`; `Create` calls `Guard.NotNull` and `Save`;
//!   `OrderService{i}.Save` calls `Audit.Log`; `OrderService{i}.Find` only
//!   calls `List.Find` (external). A resolved edge to anything else is
//!   wrong.
//! * Six how-to notes use `Find`, `Save`, `Get`, `Log` and `Create` as
//!   plain words; every link from them is wrong.
//!
//! Precision is reported per resolver and per confidence level.
//! `DOCMONK_EVAL_OUT=path` writes the report; floors are in
//! `fixtures/linking/precision-gates.json` (`DOCMONK_BLESS=1` rewrites
//! them from this run).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use docmonk_config::DocMonkConfig;
use docmonk_core::paths::Home;
use docmonk_service::bench::corpus;
use docmonk_service::query::open_sources;
use regex::Regex;
use serde_json::{json, Value};

#[derive(Default)]
struct Tally {
    correct: usize,
    wrong: usize,
}

impl Tally {
    fn add(&mut self, ok: bool) {
        if ok {
            self.correct += 1;
        } else {
            self.wrong += 1;
        }
    }
    fn json(&self) -> Value {
        let n = self.correct + self.wrong;
        json!({
            "n": n,
            "correct": self.correct,
            "precision": if n == 0 { Value::Null } else {
                json!(((self.correct as f64 / n as f64) * 10_000.0).round() / 10_000.0)
            },
        })
    }
}

fn tallies(map: &BTreeMap<String, Tally>) -> Value {
    Value::Object(map.iter().map(|(k, v)| (k.clone(), v.json())).collect())
}

/// The group number in a path like `.../OrderService12.cs` or
/// `.../Orders12Controller.cs`.
fn file_group(path: &str) -> Option<(&'static str, usize)> {
    let name = Path::new(path).file_name()?.to_str()?;
    if let Some(n) = name
        .strip_prefix("OrderService")
        .and_then(|r| r.strip_suffix(".cs"))
    {
        return Some(("service", n.parse().ok()?));
    }
    let n = name.strip_prefix("Orders")?.strip_suffix("Controller.cs")?;
    Some(("controller", n.parse().ok()?))
}

#[test]
fn link_and_edge_precision() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("shop");
    let groups = corpus::emit(&root, 90, 24);
    // Prose that uses member names as plain words: every link from these
    // sections is wrong (bare-name over-linking).
    for k in 0..6 {
        corpus::write(
            &root,
            &format!("docs/howto_{k}.md"),
            "# How to use the shop\n\n## General usage\n\nFind the order first, then Save it. \
             Get returns the details and Log records the change. Create a new order when Find \
             returns nothing, and call Save again after every edit.\n",
        );
    }
    let home = Home::new(dir.path().join("home"));
    home.ensure_layout().unwrap();
    let mut cfg = DocMonkConfig::default();
    cfg.updates.enabled = false;
    docmonk_config::write_user_config(&cfg, &home).unwrap();
    let mut cat = docmonk_service::sources::catalog(&home).unwrap();
    let canonical = std::fs::canonicalize(&root).unwrap();
    cat.add(canonical.to_str().unwrap(), vec![], vec![])
        .unwrap();
    let sources = docmonk_service::indexing::selected_sources(&home, None).unwrap();
    docmonk_service::indexing::index_sources(&home, &sources, "index", |_| {})
        .unwrap()
        .into_result()
        .unwrap();
    let opened = open_sources(&home, None).unwrap();
    let o = &opened[0];
    let store = o.store.local().unwrap();
    let build = &o.build;

    let entities: HashMap<String, docmonk_storage::knowledge::EntityRow> = store
        .all_entities(build)
        .unwrap()
        .into_iter()
        .map(|e| (e.id.clone(), e))
        .collect();
    let files: HashMap<String, String> = store
        .files(build)
        .unwrap()
        .into_iter()
        .map(|f| (f.id, f.rel_path))
        .collect();
    let path_of = |entity_id: &str| -> Option<&str> {
        entities
            .get(entity_id)
            .and_then(|e| files.get(&e.file_id))
            .map(String::as_str)
    };

    // ---- links ---------------------------------------------------------
    let heading = Regex::new(r"^Orders (\d+)$").unwrap();
    let mut by_resolver: BTreeMap<String, Tally> = BTreeMap::new();
    let mut by_confidence: BTreeMap<String, Tally> = BTreeMap::new();
    let mut found: std::collections::HashSet<(String, String)> = Default::default();
    let mut sections: Vec<(String, usize)> = Vec::new();
    for d in store.documents(build).unwrap() {
        for c in store.file_chunks(build, &d.file_id).unwrap() {
            if let Some(g) = c
                .heading_path
                .last()
                .and_then(|h| heading.captures(h))
                .and_then(|m| m[1].parse::<usize>().ok())
            {
                sections.push((c.id.clone(), g));
            }
        }
    }
    let section_group: HashMap<&str, usize> =
        sections.iter().map(|(c, g)| (c.as_str(), *g)).collect();
    let links = store.links(build).unwrap();
    for l in &links {
        let group = l.chunk_id.as_deref().and_then(|c| section_group.get(c));
        let ok = match (group, path_of(&l.entity_id).and_then(file_group)) {
            (Some(g), Some((_, eg))) => *g == eg,
            _ => false,
        };
        if ok {
            if let Some(c) = &l.chunk_id {
                found.insert((c.clone(), l.entity_id.clone()));
            }
        }
        by_resolver.entry(l.resolver.clone()).or_default().add(ok);
        by_confidence
            .entry(l.confidence.clone())
            .or_default()
            .add(ok);
    }
    // Recall over the five planted entities of every section.
    let planted = |g: usize| -> Vec<String> {
        let mut out = Vec::new();
        for e in entities.values() {
            let Some((kind, eg)) = path_of(&e.id).and_then(file_group) else {
                continue;
            };
            if eg != g {
                continue;
            }
            let want = match kind {
                "controller" => e.name == format!("Orders{g}Controller") || e.name == "Get",
                _ => {
                    (e.name == format!("OrderService{g}") && e.kind == "class")
                        || ((e.name == "Save" || e.name == "Find")
                            && e.qualified_name.contains(&format!(".OrderService{g}.")))
                }
            };
            if want {
                out.push(e.id.clone());
            }
        }
        out
    };
    let mut want = 0;
    let mut got = 0;
    for (chunk, g) in &sections {
        for e in planted(*g) {
            want += 1;
            got += usize::from(found.contains(&(chunk.clone(), e)));
        }
    }

    // ---- call edges ----------------------------------------------------
    let num = Regex::new(r"(?:Orders|OrderService)(\d+)").unwrap();
    let mut edge_resolver: BTreeMap<String, Tally> = BTreeMap::new();
    let mut edge_confidence: BTreeMap<String, Tally> = BTreeMap::new();
    let mut unlabelled = 0usize;
    for f in store.files(build).unwrap() {
        for r in store.file_relationships(build, &f.id).unwrap() {
            if r.relationship_type != "calls" {
                continue;
            }
            let Some(target) = r.target_entity_id.as_deref().and_then(|t| entities.get(t)) else {
                continue;
            };
            let Some(source) = entities.get(&r.source_entity_id) else {
                continue;
            };
            let Some(i) = num
                .captures(&source.qualified_name)
                .and_then(|m| m[1].parse::<usize>().ok())
            else {
                unlabelled += 1;
                continue;
            };
            let t = target.qualified_name.as_str();
            let svc = |m: &str| {
                t.ends_with(&format!(".OrderService{i}.{m}"))
                    || t.ends_with(&format!(".IOrderService{i}.{m}"))
            };
            let ok = match (
                source.qualified_name.contains("Controller"),
                source.name.as_str(),
            ) {
                (true, "Get") => {
                    svc("Find") || t.ends_with(&format!(".Mapper{}.ToDto", (i + 1) % groups))
                }
                (true, "Create") => svc("Save") || t.ends_with(".Guard.NotNull"),
                (false, "Save") => t.ends_with(".Audit.Log"),
                (false, "Find") => false,
                _ => {
                    unlabelled += 1;
                    continue;
                }
            };
            edge_resolver.entry(r.resolver.clone()).or_default().add(ok);
            edge_confidence
                .entry(r.confidence.clone())
                .or_default()
                .add(ok);
        }
    }

    let total = |m: &BTreeMap<String, Tally>| {
        let mut t = Tally::default();
        for v in m.values() {
            t.correct += v.correct;
            t.wrong += v.wrong;
        }
        t.json()
    };
    let report = json!({
        "corpus": {"code_files": 90, "notes": 24, "howto_notes": 6, "groups": groups, "labels": "generator-planted (exact, every link and edge)"},
        "links": {
            "total": total(&by_resolver),
            "by_resolver": tallies(&by_resolver),
            "by_confidence": tallies(&by_confidence),
            "recall": {"planted": want, "found": got, "recall": (got as f64 / want.max(1) as f64 * 10_000.0).round() / 10_000.0},
        },
        "call_edges": {
            "total": total(&edge_resolver),
            "by_resolver": tallies(&edge_resolver),
            "by_confidence": tallies(&edge_confidence),
            "unlabelled": unlabelled,
        },
    });
    if let Ok(out) = std::env::var("DOCMONK_EVAL_OUT") {
        std::fs::write(out, serde_json::to_string_pretty(&report).unwrap() + "\n").unwrap();
    }
    assert!(want > 0 && !links.is_empty(), "{report}");

    // Floors: precision and recall never fall more than `tolerance` below
    // the recorded values.
    let gates_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/linking/precision-gates.json");
    let pick = |r: &Value| -> BTreeMap<String, f64> {
        let mut m = BTreeMap::new();
        m.insert(
            "links.total".into(),
            r["links"]["total"]["precision"].as_f64().unwrap_or(0.0),
        );
        m.insert(
            "links.recall".into(),
            r["links"]["recall"]["recall"].as_f64().unwrap_or(0.0),
        );
        m.insert(
            "call_edges.total".into(),
            r["call_edges"]["total"]["precision"]
                .as_f64()
                .unwrap_or(0.0),
        );
        for (k, v) in r["links"]["by_confidence"]
            .as_object()
            .into_iter()
            .flatten()
        {
            if let Some(p) = v["precision"].as_f64() {
                m.insert(format!("links.confidence.{k}"), p);
            }
        }
        m
    };
    let now = pick(&report);
    if std::env::var("DOCMONK_BLESS").as_deref() == Ok("1") {
        std::fs::create_dir_all(gates_path.parent().unwrap()).unwrap();
        let gates = json!({
            "description": "Generator-labelled link and call-edge precision (crates/docmonk-service/tests/link_precision.rs). A value more than `tolerance` below its recorded value fails; rewrite with DOCMONK_BLESS=1 when a change is meant to move it.",
            "tolerance": 0.01,
            "floors": now,
        });
        std::fs::write(
            &gates_path,
            serde_json::to_string_pretty(&gates).unwrap() + "\n",
        )
        .unwrap();
        return;
    }
    let gates: Value =
        serde_json::from_str(&std::fs::read_to_string(&gates_path).unwrap()).unwrap();
    let tol = gates["tolerance"].as_f64().unwrap();
    for (k, floor) in gates["floors"].as_object().unwrap() {
        let floor = floor.as_f64().unwrap();
        let got = now.get(k).copied().unwrap_or(0.0);
        assert!(
            got + tol >= floor,
            "{k}: {got} < {floor} - {tol}\n{report:#}"
        );
    }
}
