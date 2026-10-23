//! The normalized lexical projection (ADR 0039) as one behaviour contract
//! for every backend: SQLite always, OpenSearch and Elasticsearch when
//! `DOCMONK_TEST_OPENSEARCH_URL` / `DOCMONK_TEST_ELASTICSEARCH_URL` point at
//! disposable clusters. Accent-free and inflected Greek, unsegmented
//! Japanese and Chinese, and Arabic without its article find their
//! documents lexically (no embedding model); exact symbols and paths rank
//! as before; snippets stay original text.

use std::path::{Path, PathBuf};

use docmonk_config::DocMonkConfig;
use docmonk_core::paths::Home;
use docmonk_retrieval::lexical::RankTier;
use docmonk_retrieval::route::SearchFilters;
use docmonk_service::query::{self, open_sources, SearchRequest};

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

struct Backend {
    name: &'static str,
    _dir: tempfile::TempDir,
    home: Home,
    root: PathBuf,
    cleanup: Option<(String, String)>,
}

impl Drop for Backend {
    fn drop(&mut self) {
        if let Some((base, prefix)) = &self.cleanup {
            use docmonk_backends::schema::{index_name, IndexKind};
            let c = reqwest::blocking::Client::new();
            for kind in IndexKind::ALL {
                let _ = c
                    .delete(format!("{base}/{}", index_name(prefix, kind)))
                    .send();
            }
        }
    }
}

fn setup(name: &'static str, server: Option<&str>) -> Backend {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::new(dir.path().join("home"));
    home.ensure_layout().unwrap();
    let mut cfg = DocMonkConfig::default();
    cfg.updates.enabled = false;
    cfg.search.semantic = false;
    let mut cleanup = None;
    if let Some(url) = server {
        let prefix = format!(
            "lexnorm{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000_000
        );
        cfg.storage.mode = "server".into();
        cfg.storage.server.engine = match name {
            "opensearch" => docmonk_config::model::StorageEngine::OpenSearch,
            _ => docmonk_config::model::StorageEngine::Elasticsearch,
        };
        cfg.storage.server.url = url.into();
        cfg.storage.server.index_prefix = prefix.clone();
        cleanup = Some((url.to_owned(), prefix));
    }
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
    assert_eq!(run.failed_sources, 0, "{name}");
    Backend {
        name,
        _dir: dir,
        home,
        root,
        cleanup,
    }
}

fn backends() -> Vec<Backend> {
    let mut out = vec![setup("sqlite", None)];
    for (name, var) in [
        ("opensearch", "DOCMONK_TEST_OPENSEARCH_URL"),
        ("elasticsearch", "DOCMONK_TEST_ELASTICSEARCH_URL"),
    ] {
        if let Some(url) = std::env::var(var).ok().filter(|u| !u.is_empty()) {
            out.push(setup(name, Some(&url)));
        }
    }
    out
}

/// Lexical hits: (relative path, tier, snippet).
fn lexical(b: &Backend, q: &str) -> Vec<(String, RankTier, Option<String>)> {
    let cfg = docmonk_service::load(&b.home).unwrap();
    let opened = open_sources(&b.home, None).unwrap();
    let run = query::run_search(
        &b.home,
        &cfg,
        &opened,
        &SearchRequest {
            query: q,
            limit: 10,
            hybrid: false,
            with_context: false,
            filters: SearchFilters::default(),
        },
    )
    .unwrap();
    let canon = docmonk_core::paths::resolve(&b.root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    let raw = b.root.to_string_lossy().replace('\\', "/");
    run.results
        .into_iter()
        .map(|r| {
            let p = r.path.replace('\\', "/");
            let rel = [canon.as_str(), raw.as_str()]
                .iter()
                .find_map(|root| p.strip_prefix(&format!("{root}/")))
                .unwrap_or(&p)
                .to_owned();
            (rel, r.tier, r.snippet)
        })
        .collect()
}

#[test]
fn normalized_forms_find_multilingual_documents_on_every_backend() {
    // (query, document): none of these queries shares a written word form
    // with its document beyond what normalization recovers.
    let cases = [
        // Greek without accents, and inflected (-ής, -ας).
        ("τηλεργασια μετα την επιστροφη", "docs/el/goniki-adeia.md"),
        ("διάρκεια γονικής αδείας", "docs/el/goniki-adeia.md"),
        ("λογαριασμού διαχειριστών", "docs/el/asfaleia-kodikon.md"),
        // Japanese and Chinese: no spaces between words.
        ("セキュリティ研修の所要時間", "docs/ja/onboarding.md"),
        ("社員証", "docs/ja/onboarding.md"),
        ("备份文件保留多少天", "docs/zh/backup.md"),
        ("恢复演练", "docs/zh/backup.md"),
        // Arabic without the article, teh marbuta and all.
        ("مصادقة ثنائية", "docs/ar/security.md"),
    ];
    for b in backends() {
        for (q, doc) in cases {
            let hits = lexical(&b, q);
            assert!(
                hits.iter().any(|(p, _, _)| p == doc),
                "{}: {q:?} should find {doc}: {hits:?}",
                b.name
            );
        }
        // Snippets are the documents' own text, never normalized forms.
        let hits = lexical(&b, "τηλεργασια μετα την επιστροφη");
        let (_, _, snippet) = hits
            .iter()
            .find(|(p, _, _)| p == "docs/el/goniki-adeia.md")
            .unwrap();
        let snippet = snippet.as_deref().unwrap_or_default();
        assert!(
            snippet.chars().any(|c| "άέήίόύώ".contains(c)),
            "{}: {snippet:?}",
            b.name
        );
        // Exact symbols and paths are unaffected.
        let hits = lexical(&b, "RefundService");
        assert_eq!(hits[0].1, RankTier::ExactSymbol, "{}: {hits:?}", b.name);
        assert_eq!(hits[0].0, "Payments/RefundService.cs");
        let hits = lexical(&b, "services/billing/invoices.py");
        assert_eq!(hits[0].0, "services/billing/invoices.py", "{}", b.name);
    }
}
