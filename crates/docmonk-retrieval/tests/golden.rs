//! Search rankings against fixtures/expected/search.json.
//!
//! The corpora are indexed with every file mtime pinned. Each expected hit
//! carries its tie key: the sort key without ids,
//! with semantic scores rounded. Consecutive hits with equal tie keys form
//! a tie group, whose members may appear in any order. The comparison is
//! therefore per group, as multisets of `[kind, tier, path, title]`. A
//! group cut off by the top-20 limit only has to match on its tie key.
//!
//! Lexical rankings are always checked. Hybrid rankings need the embedding
//! model, and the neural pass also needs the reranker (see docmonk-ml's tests
//! for the `DOCMONK_MODELS_DIR` / `DOCMONK_REQUIRE_MODELS` contract).

use std::path::{Path, PathBuf};

use docmonk_core::models::SourceType;
use docmonk_core::paths::{project_id_for_canonical, Home};
use docmonk_indexing::coordinator::{run_source, NoProgress, Options};
use docmonk_ml::embedder::Embedder;
use docmonk_ml::manifest::{DEFAULT_EMBEDDING_MODEL, DEFAULT_RERANKER_MODEL};
use docmonk_ml::reranker::CrossEncoder;
use docmonk_retrieval::hybrid::{self, HybridOptions, RankedHit};
use docmonk_retrieval::lexical::{self, SearchResult};
use docmonk_retrieval::Corpus;
use docmonk_storage::control::{ControlPlane, NewSource};
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::StorageLayout;
use serde_json::{json, Value};

const MTIME: u64 = 1_700_000_000;
const TOP: usize = 20;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn copy_pinned(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let p = e.unwrap().path();
        let dest = to.join(p.file_name().unwrap());
        if p.is_dir() {
            copy_pinned(&p, &dest);
        } else {
            std::fs::copy(&p, &dest).unwrap();
            let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(MTIME);
            std::fs::File::options()
                .write(true)
                .open(&dest)
                .unwrap()
                .set_modified(t)
                .unwrap();
        }
    }
}

fn models_root(slug: &str) -> Option<PathBuf> {
    let root = std::env::var_os("DOCMONK_MODELS_DIR").map(PathBuf::from)?;
    if root.join(slug).join("model.safetensors").exists() {
        Some(root)
    } else {
        assert_ne!(
            std::env::var("DOCMONK_REQUIRE_MODELS").as_deref(),
            Ok("1"),
            "DOCMONK_REQUIRE_MODELS=1 but {slug} is not installed"
        );
        None
    }
}

struct Indexed {
    _tmp: tempfile::TempDir,
    store: ProjectStore,
    build: String,
}

fn index(rel: &str, models: Option<&Path>) -> Indexed {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("corpus");
    copy_pinned(&repo_root().join(rel), &root);
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
    let mut cfg = docmonk_config::DocMonkConfig::default();
    cfg.search.semantic = models.is_some();
    let opts = docmonk_convert::RegistryOptions {
        models_root: models.map(Path::to_path_buf),
        ..Default::default()
    };
    run_source(
        &layout,
        &mut cp,
        &src,
        &docmonk_convert::registry_with(&cfg, &opts),
        &Options::from_config(&cfg),
        &mut NoProgress,
    )
    .unwrap();
    let build = cp.state(&src.id).unwrap().active_build_id.unwrap();
    let store =
        ProjectStore::open(&layout, &project_id_for_canonical(&src.path), &src.id, 8).unwrap();
    Indexed {
        _tmp: tmp,
        store,
        build,
    }
}

fn golden() -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(repo_root().join("fixtures/expected/search.json")).unwrap(),
    )
    .unwrap()
}

fn round5(x: Option<f64>) -> Value {
    x.map_or(Value::Null, |v| json!((v * 1e5).round() / 1e5))
}

fn lex_row(r: &SearchResult) -> (Value, Value) {
    (
        json!([r.kind, r.tier.label(), r.path, r.title]),
        json!([
            r.tier as u8,
            r.query_tier as u8,
            r.fts_rank,
            r.entity_kind_rank
        ]),
    )
}

fn hyb_row(h: &RankedHit) -> (Value, Value) {
    let c = &h.candidate;
    let sem = round5(c.semantic_score.map(f64::from));
    let key = if c.exact_match {
        json!([
            "pinned",
            c.lexical_tier.unwrap() as u8,
            c.lexical_query_tier as u8,
            c.lexical_fts_rank,
            c.entity_kind_rank,
            sem
        ])
    } else {
        json!([
            "hybrid",
            (c.rrf_score.unwrap_or(0.0) * 1e9).round() / 1e9,
            c.lexical_query_tier as u8,
            c.lexical_fts_rank,
            c.entity_kind_rank,
            sem
        ])
    };
    (json!([c.kind, h.tier_label, c.path, c.title]), key)
}

/// Expected-file form of compared rows: `[kind, tier, path, title, key]`.
fn record(rows: &[(Value, Value)]) -> Value {
    Value::Array(
        rows.iter()
            .map(|(row, key)| {
                let mut v = row.as_array().unwrap().clone();
                v.push(key.clone());
                Value::Array(v)
            })
            .collect(),
    )
}

fn bless() -> bool {
    std::env::var("DOCMONK_BLESS").as_deref() == Ok("1")
}

/// Writes the expected rankings (one-space indent, like the checked-in file).
fn write_golden(g: &Value) {
    let mut out = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(b" ");
    serde::Serialize::serialize(
        g,
        &mut serde_json::Serializer::with_formatter(&mut out, fmt),
    )
    .unwrap();
    out.push(b'\n');
    std::fs::write(repo_root().join("fixtures/expected/search.json"), out).unwrap();
}

/// Tie keys equal, allowing rounding noise in floats.
fn keys_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| keys_eq(p, q))
        }
        (Value::Number(p), Value::Number(q)) => {
            (p.as_f64().unwrap() - q.as_f64().unwrap()).abs() <= 2e-5
        }
        _ => a == b,
    }
}

/// Compares one ranked list; returns a description of the first mismatch.
fn compare(expected: &[Value], got: &[(Value, Value)]) -> Option<String> {
    if expected.len() != got.len().min(TOP) {
        return Some(format!(
            "length {} vs expected {}",
            got.len().min(TOP),
            expected.len()
        ));
    }
    let mut i = 0;
    while i < expected.len() {
        let key = &expected[i][4];
        let mut j = i;
        while j < expected.len() && keys_eq(&expected[j][4], key) {
            j += 1;
        }
        let mut want: Vec<String> = expected[i..j]
            .iter()
            .map(|e| json!(e.as_array().unwrap()[..4]).to_string())
            .collect();
        let mut have: Vec<String> = got[i..j].iter().map(|g| g.0.to_string()).collect();
        let cut = j == expected.len() && expected.len() == TOP;
        if cut {
            if let Some(k) = (i..j).find(|&k| !keys_eq(&got[k].1, key)) {
                return Some(format!("#{k}: tie key {} vs expected {key}", got[k].1));
            }
        } else {
            want.sort();
            have.sort();
            if want != have {
                return Some(format!("#{i}..{j}: got {have:?}\n      expected {want:?}"));
            }
        }
        i = j;
    }
    None
}

/// Same `[kind, tier, path, title]` sequence. The neural pass has no tie
/// keys (its logits are compared in docmonk-ml).
///
/// Path hits are compared as a set only: the expected rescoring was taken
/// with absolute-path titles, while path hits are titled with the
/// source-relative path, so their rescored positions legitimately differ.
fn compare_neural(expected: &[Value], got: &[Value]) -> Option<String> {
    let split = |v: &[Value]| -> (Vec<String>, Vec<String>) {
        let (mut paths, mut rest) = (Vec::new(), Vec::new());
        for x in v {
            if x[0] == "path" {
                paths.push(x.to_string());
            } else {
                rest.push(x.to_string());
            }
        }
        paths.sort();
        (paths, rest)
    };
    let (want_paths, want) = split(expected);
    let (have_paths, have) = split(got);
    (want != have || want_paths != have_paths)
        .then(|| format!("got {have:?} {have_paths:?}\n      expected {want:?} {want_paths:?}"))
}

fn report(failures: &[String], total: usize, what: &str) {
    for f in failures {
        eprintln!("{f}");
    }
    assert!(
        failures.is_empty(),
        "{what}: {} of {total} queries differ from the expected rankings",
        failures.len()
    );
}

const CORPORA: [(&str, &str); 2] = [
    ("search_quality", "fixtures/search_quality/project"),
    ("linking", "fixtures/linking"),
];

#[test]
fn lexical_rankings_are_as_expected() {
    // With DOCMONK_BLESS=1 run with --test-threads=1: both ranking tests
    // rewrite the same file.
    let mut g = golden();
    let mut failures = Vec::new();
    let mut total = 0;
    for (name, rel) in CORPORA {
        let ix = index(rel, None);
        let corpora = [Corpus {
            store: &ix.store,
            build_id: &ix.build,
        }];
        for item in g[name].as_array_mut().unwrap() {
            total += 1;
            let q = item["query"].as_str().unwrap().to_owned();
            let q = q.as_str();
            let got: Vec<_> = lexical::search(&corpora, q, 50, 32)
                .unwrap()
                .iter()
                .take(TOP)
                .map(lex_row)
                .collect();
            if bless() {
                item["lexical"] = record(&got);
                continue;
            }
            if let Some(m) = compare(item["lexical"].as_array().unwrap(), &got) {
                failures.push(format!("[{name}] {q:?}: {m}"));
            }
        }
    }
    if bless() {
        write_golden(&g);
    }
    report(&failures, total, "lexical");
}

#[test]
fn hybrid_and_neural_rankings_are_as_expected() {
    let Some(models) = models_root(DEFAULT_EMBEDDING_MODEL.slug) else {
        return;
    };
    let emb = Embedder::load(
        &models.join(DEFAULT_EMBEDDING_MODEL.slug),
        DEFAULT_EMBEDDING_MODEL,
    )
    .unwrap();
    let ce = models_root(DEFAULT_RERANKER_MODEL.slug).map(|m| {
        CrossEncoder::load(&m.join(DEFAULT_RERANKER_MODEL.slug), DEFAULT_RERANKER_MODEL).unwrap()
    });
    let mut g = golden();
    let bless = std::env::var("DOCMONK_BLESS").as_deref() == Ok("1");
    let mut failures = Vec::new();
    let mut neural_failures = Vec::new();
    let mut total = 0;
    for (name, rel) in CORPORA {
        let ix = index(rel, Some(&models));
        let items = g[name].as_array_mut().unwrap();
        let corpora = [Corpus {
            store: &ix.store,
            build_id: &ix.build,
        }];
        for item in items.iter_mut() {
            total += 1;
            let q = item["query"].as_str().unwrap().to_owned();
            let q = q.as_str();
            let opts = HybridOptions {
                limit: TOP,
                ..Default::default()
            };
            let r = hybrid::hybrid_search(&corpora, q, Some(&emb), None, opts).unwrap();
            let got: Vec<_> = r.hits.iter().map(hyb_row).collect();
            if bless {
                item["hybrid"] = record(&got);
            } else if let Some(m) = compare(item["hybrid"].as_array().unwrap(), &got) {
                failures.push(format!("[{name}] {q:?}: {m}"));
            }
            if let Some(ce) = &ce {
                let neural = hybrid::neural_rerank(q, r.hits, TOP, Some(ce));
                let got: Vec<Value> = neural.iter().map(|h| hyb_row(h).0).collect();
                if bless {
                    item["hybrid_neural"] = json!(got);
                    continue;
                }
                if let Some(m) = compare_neural(item["hybrid_neural"].as_array().unwrap(), &got) {
                    neural_failures.push(format!("[{name}] {q:?}: {m}"));
                }
            }
        }
    }
    if bless {
        write_golden(&g);
    }
    report(&failures, total, "hybrid");
    report(&neural_failures, total, "hybrid + neural");
}

#[test]
fn hybrid_degrades_to_lexical_when_semantic_is_unavailable() {
    let ix = index("fixtures/linking", None);
    let corpora = [Corpus {
        store: &ix.store,
        build_id: &ix.build,
    }];
    let opts = HybridOptions {
        limit: TOP,
        rerank_top_n: Some(5),
        ..Default::default()
    };
    let r = hybrid::hybrid_search(&corpora, "cancel order", None, None, opts).unwrap();
    assert!(!r.semantic.available);
    assert_eq!(r.semantic.reason, "embedding model unavailable");
    let expected = hybrid::rerank(hybrid::merge(&r.lexical, &[]), TOP);
    assert_eq!(
        r.hits, expected,
        "no model: lexical-only fusion, neural pass skipped"
    );
    assert!(r.hits.iter().all(|h| h.neural_score.is_none()));
    // Exact matches stay pinned above fused hits.
    let r = hybrid::hybrid_search(&corpora, "InvoiceService", None, None, opts).unwrap();
    assert_eq!(r.hits[0].tier_label, "exact_symbol");
    assert!(r.hits[1..].iter().all(|h| h.tier_label == "hybrid"));
    // Empty and token-less queries return nothing rather than failing.
    for q in ["", "   ", "$$$ (( ))"] {
        let r = hybrid::hybrid_search(&corpora, q, None, None, opts).unwrap();
        assert!(r.lexical.iter().all(|h| h.kind == "path"), "{q:?}");
    }
}

/// Latency of lexical search over the 72 golden queries (5 rounds).
/// Informational; run with `--release --ignored --nocapture`.
#[test]
#[ignore]
fn lexical_latency() {
    let ix = index("fixtures/search_quality/project", None);
    let corpora = [Corpus {
        store: &ix.store,
        build_id: &ix.build,
    }];
    let g = golden();
    let qs: Vec<&str> = g["search_quality"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["query"].as_str().unwrap())
        .collect();
    for q in &qs {
        lexical::search(&corpora, q, TOP, 32).unwrap();
    }
    let mut ms = Vec::new();
    for _ in 0..5 {
        for q in &qs {
            let t = std::time::Instant::now();
            lexical::search(&corpora, q, TOP, 32).unwrap();
            ms.push(t.elapsed().as_secs_f64() * 1000.0);
        }
    }
    ms.sort_by(f64::total_cmp);
    eprintln!(
        "lexical p50 {:.3} p95 {:.3}",
        ms[ms.len() / 2],
        ms[ms.len() * 95 / 100]
    );
}
