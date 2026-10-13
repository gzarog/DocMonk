//! Attachment provenance on search hits against
//! fixtures/expected/attachments.json: lexical hits always; semantic hits
//! when the embedding model is installed.

use std::path::{Path, PathBuf};

use docmonk_core::models::SourceType;
use docmonk_core::paths::{project_id_for_canonical, Home};
use docmonk_indexing::coordinator::{run_source, NoProgress, Options};
use docmonk_ml::embedder::Embedder;
use docmonk_ml::manifest::DEFAULT_EMBEDDING_MODEL;
use docmonk_retrieval::hybrid;
use docmonk_retrieval::lexical;
use docmonk_retrieval::Corpus;
use docmonk_storage::control::{ControlPlane, NewSource};
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::StorageLayout;
use serde_json::{json, Value};

const MTIME: u64 = 1_700_000_000;

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

fn attachment(a: Option<&docmonk_storage::search::AttachmentProvenance>) -> Value {
    a.map_or(Value::Null, |a| {
        json!({
            "name": a.name,
            "content_type": a.content_type,
            "index": a.index,
            "format": a.format,
            "parent_title": a.parent_title,
        })
    })
}

#[test]
fn attachment_hits_carry_parent_provenance() {
    let golden: Value = serde_json::from_str(
        &std::fs::read_to_string(repo_root().join("fixtures/expected/attachments.json")).unwrap(),
    )
    .unwrap();
    let models = models_root(DEFAULT_EMBEDDING_MODEL.slug);
    let ix = index("fixtures/attachments", models.as_deref());
    let corpora = [Corpus {
        store: &ix.store,
        build_id: &ix.build,
    }];
    let emb = models.as_ref().map(|m| {
        Embedder::load(
            &m.join(DEFAULT_EMBEDDING_MODEL.slug),
            DEFAULT_EMBEDDING_MODEL,
        )
        .unwrap()
    });
    for item in golden["queries"].as_array().unwrap() {
        let q = item["query"].as_str().unwrap();
        let got: Vec<Value> = lexical::search(&corpora, q, 20, 32)
            .unwrap()
            .iter()
            .map(|r| {
                let a = r.location.as_ref().and_then(|l| l.attachment.as_ref());
                json!([r.kind, r.tier.label(), attachment(a)])
            })
            .collect();
        assert_eq!(json!(got), item["lexical"], "lexical {q:?}");
        if let Some(emb) = &emb {
            let s = hybrid::semantic_search(&corpora, Some(emb), q, 5, None).unwrap();
            let mut got: Vec<String> = s
                .hits
                .iter()
                .map(|h| json!([h.hit.kind, attachment(h.attachment.as_ref())]).to_string())
                .collect();
            let mut want: Vec<String> = item["semantic"]
                .as_array()
                .unwrap()
                .iter()
                .map(Value::to_string)
                .collect();
            got.sort();
            want.sort();
            assert_eq!(got, want, "semantic {q:?}");
        }
    }
}
