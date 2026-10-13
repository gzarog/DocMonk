//! Retrieval quality gates on 72 golden queries over a mixed code and
//! document project (fixtures/search_quality): lexical search always, and
//! hybrid (lexical + semantic) when the embedding model is installed.
//!
//! Metrics are Recall@1/3/5/10, MRR and NDCG@10 over `kind:path` keys, in
//! total and per query category; each must reach its floor in
//! fixtures/search_quality/gates.json. `RAGMONK_QUALITY_OUT=<file>` writes
//! the measured numbers.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use docmonk_core::models::SourceType;
use docmonk_core::paths::{project_id_for_canonical, Home};
use docmonk_indexing::coordinator::{run_source, NoProgress, Options};
use docmonk_ml::embedder::Embedder;
use docmonk_ml::manifest::DEFAULT_EMBEDDING_MODEL;
use docmonk_retrieval::{hybrid, lexical, Corpus};
use docmonk_storage::control::{ControlPlane, NewSource};
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::StorageLayout;
use serde_json::{json, Value};

const K: usize = 10;

/// Per query: ranked `kind:path` keys and the relevant keys.
type Runs = Vec<(Vec<String>, HashSet<String>)>;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

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

fn models_root() -> Option<PathBuf> {
    let root = std::env::var_os("RAGMONK_MODELS_DIR").map(PathBuf::from)?;
    let ok = root
        .join(DEFAULT_EMBEDDING_MODEL.slug)
        .join("model.safetensors")
        .exists();
    assert!(
        ok || std::env::var("RAGMONK_REQUIRE_MODELS").as_deref() != Ok("1"),
        "RAGMONK_REQUIRE_MODELS=1 but the embedding model is not installed"
    );
    ok.then_some(root)
}

fn metrics(runs: &Runs) -> BTreeMap<String, f64> {
    let n = runs.len() as f64;
    let mut m = BTreeMap::new();
    for k in [1, 3, 5, 10] {
        let r: f64 = runs
            .iter()
            .map(|(got, rel)| {
                let top: HashSet<&String> = got.iter().take(k).collect();
                rel.iter().filter(|x| top.contains(x)).count() as f64 / rel.len() as f64
            })
            .sum();
        m.insert(format!("recall@{k}"), r / n);
    }
    let mrr: f64 = runs
        .iter()
        .map(|(got, rel)| {
            got.iter()
                .position(|g| rel.contains(g))
                .map_or(0.0, |i| 1.0 / (i as f64 + 1.0))
        })
        .sum();
    m.insert("mrr".into(), mrr / n);
    let ndcg: f64 = runs
        .iter()
        .map(|(got, rel)| {
            let dcg: f64 = got
                .iter()
                .take(K)
                .enumerate()
                .filter(|(_, g)| rel.contains(*g))
                .map(|(i, _)| 1.0 / (i as f64 + 2.0).log2())
                .sum();
            let ideal: f64 = (0..rel.len().min(K))
                .map(|i| 1.0 / (i as f64 + 2.0).log2())
                .sum();
            dcg / ideal
        })
        .sum();
    m.insert("ndcg@10".into(), ndcg / n);
    m
}

/// Ranked `kind:path` keys without duplicates.
fn dedup(keys: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    keys.filter(|k| seen.insert(k.clone())).collect()
}

#[test]
fn retrieval_meets_the_quality_gates() {
    let models = models_root();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    copy_tree(&repo_root().join("fixtures/search_quality/project"), &root);
    let home = Home::new(tmp.path().join("home"));
    let layout = StorageLayout::new(&home);
    let mut cp = ControlPlane::open(&layout, 8).unwrap();
    let canonical = docmonk_core::paths::resolve(&root)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let src = cp
        .add_source(&NewSource {
            canonical_path: canonical,
            source_type: SourceType::Local,
            enabled: true,
            include_patterns: vec![],
            exclude_patterns: vec![],
        })
        .unwrap()
        .0;
    let mut cfg = docmonk_config::RagMonkConfig::default();
    cfg.search.semantic = models.is_some();
    let reg_opts = docmonk_convert::RegistryOptions {
        models_root: models.clone(),
        ..Default::default()
    };
    run_source(
        &layout,
        &mut cp,
        &src,
        &docmonk_convert::registry_with(&cfg, &reg_opts),
        &Options::from_config(&cfg),
        &mut NoProgress,
    )
    .unwrap();
    let build = cp.state(&src.id).unwrap().active_build_id.unwrap();
    let store =
        ProjectStore::open(&layout, &project_id_for_canonical(&src.path), &src.id, 8).unwrap();
    let corpora = [Corpus {
        store: &store,
        build_id: &build,
    }];
    let embedder = models.as_ref().map(|m| {
        Embedder::load(
            &m.join(DEFAULT_EMBEDDING_MODEL.slug),
            DEFAULT_EMBEDDING_MODEL,
        )
        .unwrap()
    });

    let read = |rel: &str| -> Value {
        serde_json::from_str(&std::fs::read_to_string(repo_root().join(rel)).unwrap()).unwrap()
    };
    let golden = read("fixtures/search_quality/queries.json");
    let gates = read("fixtures/search_quality/gates.json");

    let mut arms: BTreeMap<&str, BTreeMap<String, Runs>> = BTreeMap::new();
    for item in golden["queries"].as_array().unwrap() {
        let q = item["query"].as_str().unwrap();
        let category = item["category"].as_str().unwrap().to_owned();
        let rel: HashSet<String> = item["expected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                format!(
                    "{}:{}",
                    e["kind"].as_str().unwrap(),
                    e["path"].as_str().unwrap()
                )
            })
            .collect();
        let lex = lexical::search(&corpora, q, 20, 32).unwrap();
        let got = dedup(lex.iter().map(|r| format!("{}:{}", r.kind, r.path)));
        for key in [category.clone(), "all".into()] {
            arms.entry("lexical")
                .or_default()
                .entry(key)
                .or_default()
                .push((got.clone(), rel.clone()));
        }
        if let Some(emb) = &embedder {
            let sem = hybrid::semantic_search(&corpora, Some(emb), q, 20, Some(50)).unwrap();
            let ranked = hybrid::rerank(hybrid::merge(&lex, &sem.hits), 20);
            let got = dedup(
                ranked
                    .iter()
                    .map(|h| format!("{}:{}", h.candidate.kind, h.candidate.path)),
            );
            for key in [category, "all".into()] {
                arms.entry("hybrid")
                    .or_default()
                    .entry(key)
                    .or_default()
                    .push((got.clone(), rel.clone()));
            }
        }
    }

    let measured: BTreeMap<&str, BTreeMap<String, BTreeMap<String, f64>>> = arms
        .iter()
        .map(|(arm, by_cat)| {
            (
                *arm,
                by_cat
                    .iter()
                    .map(|(c, runs)| (c.clone(), metrics(runs)))
                    .collect(),
            )
        })
        .collect();
    eprintln!("{}", serde_json::to_string_pretty(&measured).unwrap());
    if let Some(out) = std::env::var_os("RAGMONK_QUALITY_OUT") {
        std::fs::write(out, serde_json::to_string_pretty(&json!(measured)).unwrap()).unwrap();
    }

    let mut failures = Vec::new();
    for (arm, by_cat) in &measured {
        let Some(arm_gates) = gates[arm].as_object() else {
            continue;
        };
        for (cat, floors) in arm_gates {
            for (metric, floor) in floors.as_object().unwrap() {
                let got = by_cat[cat][metric];
                let floor = floor.as_f64().unwrap();
                if got < floor {
                    failures.push(format!("{arm}/{cat} {metric}: {got:.4} < {floor:.2}"));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "quality gates failed:\n{}",
        failures.join("\n")
    );
}
