//! The shared ranking path (ADR 0038) on the held-out corpus: `search
//! --hybrid`, `evidence` and `explore` cite the same top hit for the same
//! question, every surface reports the reranker's state, and context is
//! expanded around code hits as well as documents.

use std::path::{Path, PathBuf};

use docmonk_config::DocMonkConfig;
use docmonk_core::paths::Home;
use docmonk_ml::semantic::SemanticHit;
use docmonk_retrieval::context::ContextOptions;
use docmonk_retrieval::explore::Budget;
use docmonk_retrieval::hybrid::{self, SourcedHit};
use docmonk_retrieval::pipeline;
use docmonk_retrieval::route::SearchFilters;
use docmonk_service::evidence::{evidence_value, EvidenceRequest};
use docmonk_service::query::{self, corpora, open_sources, SearchRequest};
use serde_json::Value;

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let p = e.unwrap().path();
        let dest = to.join(p.file_name().unwrap());
        if p.is_dir() {
            copy_dir(&p, &dest);
        } else {
            std::fs::copy(&p, &dest).unwrap();
        }
    }
}

struct Env {
    _dir: tempfile::TempDir,
    home: Home,
    root: PathBuf,
}

fn setup() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::new(dir.path().join("home"));
    home.ensure_layout().unwrap();
    let mut cfg = DocMonkConfig::default();
    cfg.updates.enabled = false;
    // Deterministic fusion: lexical candidates only, whatever models the
    // machine has.
    cfg.search.semantic = false;
    docmonk_config::write_user_config(&cfg, &home).unwrap();
    let root = dir.path().join("corpus");
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/eval/corpus"),
        &root,
    );
    docmonk_service::sources::catalog(&home)
        .unwrap()
        .add(root.to_str().unwrap(), vec![], vec![])
        .unwrap();
    let sources = docmonk_service::indexing::selected_sources(&home, None).unwrap();
    let run = docmonk_service::indexing::index_sources(&home, &sources, "index", |_| {}).unwrap();
    assert_eq!(run.failed_sources, 0);
    Env {
        _dir: dir,
        home,
        root,
    }
}

/// `path` relative to the corpus root.
fn rel(env: &Env, p: &str) -> String {
    let p = p.replace('\\', "/");
    let mut roots = vec![env.root.to_string_lossy().replace('\\', "/")];
    if let Ok(c) = docmonk_core::paths::resolve(&env.root) {
        roots.insert(0, c.to_string_lossy().replace('\\', "/"));
    }
    for r in roots {
        if let Some(s) = p.strip_prefix(&format!("{r}/")) {
            return s.to_owned();
        }
    }
    p
}

type Top = (String, Option<i64>);

fn search_top(env: &Env, cfg: &DocMonkConfig, q: &str) -> (Top, Value) {
    let opened = open_sources(&env.home, None).unwrap();
    let run = query::run_search(
        &env.home,
        cfg,
        &opened,
        &SearchRequest {
            query: q,
            limit: 10,
            hybrid: true,
            with_context: true,
            filters: SearchFilters::default(),
        },
    )
    .unwrap();
    let ranked = run.ranked.as_ref().expect("a hybrid ranking");
    let h = &ranked[0];
    let top = (
        rel(env, &h.candidate.path),
        h.candidate.location.as_ref().and_then(|l| l.line_start),
    );
    (top, serde_json::to_value(&run.reranker).unwrap())
}

fn evidence(env: &Env, cfg: &DocMonkConfig, q: &str) -> Value {
    evidence_value(
        &env.home,
        cfg,
        &EvidenceRequest {
            query: q,
            filters: SearchFilters::default(),
            limit: 10,
        },
    )
    .unwrap()
}

fn evidence_top(env: &Env, v: &Value) -> Top {
    let e = &v["evidence"][0];
    (
        rel(env, e["path"].as_str().unwrap()),
        e["line_start"].as_i64(),
    )
}

fn explore(env: &Env, cfg: &DocMonkConfig, q: &str) -> Value {
    let opened = open_sources(&env.home, None).unwrap();
    query::explore_value(&env.home, cfg, &opened, q, Budget::default()).unwrap()
}

fn explore_top(env: &Env, v: &Value) -> Top {
    let e = &v["evidence"][0];
    (
        rel(env, e["path"].as_str().unwrap()),
        e["location"]["line_start"].as_i64(),
    )
}

#[test]
fn search_evidence_and_explore_share_one_ranking() {
    let env = setup();
    let mut cfg = docmonk_service::load(&env.home).unwrap();
    cfg.search.reranker.enabled = false;
    // Questions about what code does and what documents say: no symbol is
    // named, so each surface's evidence is the fused ranking itself.
    for q in [
        "which header prevents a retried refund request from refunding twice",
        "where are yen amounts rounded to whole units during reconciliation",
        "what tolerance in cents is allowed when matching bank lines",
        "how quickly must on-call acknowledge a SEV1 page",
        "who must approve business class flights",
    ] {
        let (search, reranker) = search_top(&env, &cfg, q);
        assert_eq!(reranker["state"], "disabled", "{q}");
        let ev = evidence(&env, &cfg, q);
        assert_eq!(evidence_top(&env, &ev), search, "{q}: evidence vs search");
        assert_eq!(ev["diagnostics"]["reranker"]["state"], "disabled", "{q}");
        let ex = explore(&env, &cfg, q);
        assert_eq!(explore_top(&env, &ex), search, "{q}: explore vs search");
        // Every citation re-verifies, and says how it was ranked.
        assert_eq!(ev["provenance"]["invalid"], 0, "{q}: {}", ev["provenance"]);
        for e in ev["evidence"].as_array().unwrap() {
            assert!(e["content_fingerprint"].is_string(), "{e}");
            assert!(e["retrieval"].is_object(), "{e}");
        }
    }
}

#[test]
fn the_reranker_state_is_reported_never_implied() {
    let env = setup();
    let mut cfg = docmonk_service::load(&env.home).unwrap();
    cfg.search.reranker.enabled = true;
    let expected = if query::cross_encoder(&env.home).is_some() {
        "applied"
    } else {
        "unavailable"
    };
    // A question with many unpinned candidates (a lone hit would be
    // `nothing_to_rescore`).
    let q = "late fees and payment reminders on invoices";
    let (_, reranker) = search_top(&env, &cfg, q);
    assert_eq!(reranker["state"], expected);
    assert_eq!(reranker["enabled"], true);
    let ev = evidence(&env, &cfg, q);
    assert_eq!(ev["diagnostics"]["reranker"]["state"], expected);
    if expected == "unavailable" {
        assert!(
            ev["diagnostics"]["degraded"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d == "reranker unavailable"),
            "{}",
            ev["diagnostics"]
        );
    }
}

#[test]
fn context_is_expanded_around_code_and_semantic_only_hits() {
    let env = setup();
    let opened = open_sources(&env.home, None).unwrap();
    let corp = corpora(&opened);
    let c = &corp[0];
    let store = c.store;
    // The long reconciliation function: a semantic-only hit on one of its
    // body passages (as a vector search reports it) gets that passage and
    // its neighbours as context.
    let run = query::run_search(
        &env.home,
        &docmonk_service::load(&env.home).unwrap(),
        &opened,
        &SearchRequest {
            query: "yen has no minor unit",
            limit: 1,
            hybrid: false,
            with_context: false,
            filters: SearchFilters::default(),
        },
    )
    .unwrap();
    let top = &run.results[0];
    let (start, end) = {
        let l = top.location.as_ref().unwrap();
        (l.line_start.unwrap(), l.line_end.unwrap())
    };
    let passages = store.entity_passages(c.build_id, &top.id).unwrap();
    assert!(passages.len() > 1, "a split body");
    let semantic = SourcedHit {
        source_id: store.source_id().to_owned(),
        hit: SemanticHit {
            kind: "entity".into(),
            subject_type: "passage".into(),
            id: top.id.clone(),
            title: top.title.clone(),
            path: top.path.clone(),
            score: 0.5,
            snippet: "yen".into(),
            section: None,
            position: start,
            end_line: Some(end),
            attachment_index: None,
            document_id: None,
        },
        attachment: None,
    };
    let ranked = hybrid::rerank(hybrid::merge(&[], &[semantic]), 1);
    assert_eq!(ranked[0].candidate.lexical_rank, None, "semantic only");
    let ctx = pipeline::expand_context(
        &corp,
        &ranked[0],
        &ContextOptions {
            parent_heading: true,
            previous_chunks: 1,
            next_chunks: 1,
            max_tokens: 2000,
        },
    )
    .unwrap()
    .expect("code context");
    assert_eq!(ctx["matched"]["line_start"], start);
    assert!(ctx["matched"]["text"].as_str().unwrap().contains("JPY"));
    let neighbours =
        ctx["previous"].as_array().unwrap().len() + ctx["next"].as_array().unwrap().len();
    assert!(neighbours >= 1, "{ctx}");
    assert!(ctx["owner"]
        .as_str()
        .unwrap()
        .contains("reconcile_ledger_entries"));
}
