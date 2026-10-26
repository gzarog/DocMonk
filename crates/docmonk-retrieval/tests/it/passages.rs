//! Implementation passages in lexical retrieval (ADR 0037), on the
//! held-out corpus: a body detail finds its owning function at the
//! passage's lines, unparsed code is found as a `passage` hit, an exact
//! symbol query keeps its symbol first without its usages, and kind
//! filters separate the two.

use std::path::Path;

use docmonk_core::models::SourceType;
use docmonk_core::paths::{project_id_for_canonical, Home};
use docmonk_indexing::coordinator::{run_source, NoProgress, Options};
use docmonk_retrieval::lexical::{self, LexicalOptions, RankTier, SearchResult};
use docmonk_retrieval::Corpus;
use docmonk_storage::control::{ControlPlane, NewSource};
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::StorageLayout;

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

struct Indexed {
    _dir: tempfile::TempDir,
    store: ProjectStore,
    build: String,
}

fn index() -> Indexed {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("corpus");
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/eval/corpus"),
        &root,
    );
    let home = Home::new(dir.path().join("home"));
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
    let cfg = docmonk_config::DocMonkConfig::default();
    let r = run_source(
        &layout,
        &mut cp,
        &src,
        &docmonk_convert::registry_with(&cfg, &Default::default()),
        &Options::from_config(&cfg),
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(r.failed, 0, "{r:?}");
    assert_eq!(r.degraded, 1, "the source file with syntax errors");
    let build = cp.state(&src.id).unwrap().active_build_id.unwrap();
    let store =
        ProjectStore::open(&layout, &project_id_for_canonical(&src.path), &src.id, 8).unwrap();
    Indexed {
        _dir: dir,
        store,
        build,
    }
}

fn search(ix: &Indexed, q: &str, kinds: &[&str]) -> Vec<SearchResult> {
    let corpora = [Corpus {
        store: &ix.store,
        build_id: &ix.build,
    }];
    let opts = LexicalOptions {
        kinds: kinds.iter().map(|k| (*k).to_owned()).collect(),
        ..Default::default()
    };
    lexical::search_with_options(&corpora, q, 10, 32, &opts)
        .unwrap()
        .0
}

fn lines(r: &SearchResult) -> (i64, i64) {
    let l = r.location.as_ref().unwrap();
    (l.line_start.unwrap(), l.line_end.unwrap())
}

#[test]
fn passages_reach_bodies_text_and_keep_symbols_first() {
    let ix = index();

    // A detail only the body holds: the owning method, at the passage's
    // lines (the cited span covers the header write).
    let hits = search(&ix, "X-Idempotency-Key", &[]);
    let top = &hits[0];
    assert_eq!(
        (top.kind, top.title.as_str()),
        ("entity", "Shop.Payments.RefundService.Issue"),
        "{hits:?}"
    );
    let (s, e) = lines(top);
    assert!(s <= 32 && 32 <= e && e - s < 40, "{s}..{e}");
    assert!(top.body);
    assert!(top
        .snippet
        .as_deref()
        .unwrap()
        .contains("X-Idempotency-Key"));

    // The large function: the JPY rounding sits in one bounded part.
    let hits = search(&ix, "yen has no minor unit", &[]);
    let top = &hits[0];
    assert_eq!(top.path, "services/billing/invoices.py");
    let (s, e) = lines(top);
    assert!(s <= 111 && 111 <= e && e - s < 40, "{s}..{e}");

    // Code without a grammar and code with syntax errors: `passage` hits.
    let hits = search(&ix, "scan_each sess", &[]);
    let rb = hits
        .iter()
        .find(|r| r.path == "scripts/cleanup.rb")
        .expect("the Ruby script is searchable");
    assert_eq!(rb.kind, "passage");
    assert!(!rb.body);
    let hits = search(&ix, "tax-dropbox.example.internal", &[]);
    assert_eq!(hits[0].path, "services/billing/tax_export.py");
    assert_eq!(hits[0].kind, "passage");

    // An exact symbol: pinned first, and the bodies that merely mention it
    // (its usages) are not text matches.
    let hits = search(&ix, "RefundService", &[]);
    assert_eq!(hits[0].tier, RankTier::ExactSymbol);
    assert!(hits.iter().all(|r| !r.body), "{hits:?}");

    // Kind filters: owned body passages are entities, text is `passage`.
    let only_entities = search(&ix, "scan_each sess X-Idempotency-Key", &["entity"]);
    assert!(only_entities.iter().all(|r| r.kind == "entity"));
    let only_text = search(&ix, "scan_each sess", &["passage"]);
    assert!(!only_text.is_empty());
    assert!(only_text.iter().all(|r| r.kind == "passage"));
}
