//! Switching `search.embedding_model` (ADR 0040) is explicit and complete:
//! the model is part of every file's identity, so the next index run is
//! a full rebuild; afterwards every vector carries the new model's
//! fingerprint, none of the old one is left, and semantic search answers
//! with the new model only. The switch itself needs both models under
//! `DOCMONK_MODELS_DIR` and is skipped otherwise.

use std::path::{Path, PathBuf};

use docmonk_config::DocMonkConfig;
use docmonk_core::paths::Home;
use docmonk_ml::manifest::{ALL_MINILM_L6_V2, MULTILINGUAL_E5_SMALL};
use docmonk_service::query::{open_sources, semantic_value};

fn models() -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var_os("DOCMONK_MODELS_DIR")?);
    [ALL_MINILM_L6_V2.slug, MULTILINGUAL_E5_SMALL.slug]
        .iter()
        .all(|s| root.join(s).join("model.safetensors").is_file())
        .then_some(root)
}

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

fn configure(home: &Home, model: &str) {
    let mut cfg = DocMonkConfig::default();
    cfg.updates.enabled = false;
    cfg.search.semantic = true;
    cfg.search.embedding_model = model.into();
    docmonk_config::write_user_config(&cfg, home).unwrap();
}

fn index(home: &Home) -> docmonk_service::indexing::RunSummary {
    let s = docmonk_service::indexing::selected_sources(home, None).unwrap();
    docmonk_service::indexing::index_sources(home, &s, "index", |_| {}).unwrap()
}

/// (vectors under `fingerprint`, vectors under any other) of the source.
fn vectors(home: &Home, fingerprint: &str) -> (usize, i64) {
    let opened = open_sources(home, None).unwrap();
    let o = &opened[0];
    let store = o.store.local().unwrap();
    (
        store.embedding_keys(&o.build, fingerprint).unwrap().len(),
        store.stale_embedding_count(&o.build, fingerprint).unwrap(),
    )
}

#[test]
fn the_model_is_part_of_the_build_identity() {
    let mut cfg = DocMonkConfig::default();
    let id = |cfg: &DocMonkConfig| {
        docmonk_convert::registry_with(cfg, &Default::default())
            .versions
            .embedding_model_id
    };
    let minilm = id(&cfg);
    cfg.search.semantic = !cfg.search.semantic;
    assert_eq!(
        id(&cfg),
        minilm,
        "toggling semantic search rebuilds nothing"
    );
    cfg.search.embedding_model = MULTILINGUAL_E5_SMALL.slug.into();
    assert_ne!(id(&cfg), minilm, "another model is another identity");
    assert!(id(&cfg)
        .unwrap()
        .ends_with(&MULTILINGUAL_E5_SMALL.fingerprint()));
}

#[test]
fn switching_models_re_embeds_everything_and_mixes_nothing() {
    let Some(_models) = models() else {
        eprintln!("skipping: both embedding models are needed under DOCMONK_MODELS_DIR");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let home = Home::new(dir.path().join("home"));
    home.ensure_layout().unwrap();
    let root = dir.path().join("src");
    write(
        &root,
        "docs/leave.md",
        "# Γονική άδεια\n\nΗ γονική άδεια διαρκεί δεκατέσσερις εβδομάδες.\n",
    );
    write(
        &root,
        "docs/refunds.md",
        "# Refunds\n\nRefunds above 500 euros need approval.\n",
    );
    write(
        &root,
        "app.py",
        "def refund(order):\n    return order.total\n",
    );
    configure(&home, ALL_MINILM_L6_V2.slug);
    docmonk_service::sources::catalog(&home)
        .unwrap()
        .add(root.to_str().unwrap(), vec![], vec![])
        .unwrap();
    index(&home);
    let old = ALL_MINILM_L6_V2.fingerprint();
    let (n_old, _) = vectors(&home, &old);
    assert!(n_old > 0);

    configure(&home, MULTILINGUAL_E5_SMALL.slug);
    let run = index(&home);
    assert_eq!(run.failed_sources, 0);
    let new = MULTILINGUAL_E5_SMALL.fingerprint();
    let (n_new, others) = vectors(&home, &new);
    assert_eq!(n_new, n_old, "every subject re-embedded");
    assert_eq!(others, 0, "no vector of the old model is left");

    let cfg = docmonk_service::load(&home).unwrap();
    let opened = open_sources(&home, None).unwrap();
    let v = semantic_value(&home, &cfg, &opened, "how long is parental leave", 3).unwrap();
    assert_eq!(v["available"], true, "{v}");
    assert!(
        v["results"][0]["path"]
            .as_str()
            .unwrap()
            .ends_with("docs/leave.md"),
        "the multilingual model finds the Greek document: {v}"
    );
}
