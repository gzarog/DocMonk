//! Search context expansion over a real indexed Markdown document:
//! enclosing heading, nearest siblings under it, table rendering and the
//! token budget.

use docmonk_core::models::SourceType;
use docmonk_core::paths::{project_id_for_canonical, Home};
use docmonk_indexing::coordinator::{run_source, NoProgress, Options};
use docmonk_retrieval::context::{expand_chunk_context, ContextOptions};
use docmonk_storage::control::{ControlPlane, NewSource};
use docmonk_storage::knowledge::{ChunkRow, ProjectStore};
use docmonk_storage::StorageLayout;

const DOC: &str = "# Guide\n\nIntro paragraph.\n\n## Setup\n\nStep one text.\n\nStep two text.\n\nStep three text.\n\n| Key | Value |\n| --- | --- |\n| a | 1 |\n\n## Usage\n\nUse it well.\n";

fn index() -> (tempfile::TempDir, ProjectStore, String) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("corpus");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("guide.md"), DOC).unwrap();
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
    let cfg = docmonk_config::RagMonkConfig::default();
    run_source(
        &layout,
        &mut cp,
        &src,
        &docmonk_convert::registry_with(&cfg, &Default::default()),
        &Options::from_config(&cfg),
        &mut NoProgress,
    )
    .unwrap();
    let build = cp.state(&src.id).unwrap().active_build_id.unwrap();
    let store =
        ProjectStore::open(&layout, &project_id_for_canonical(&src.path), &src.id, 8).unwrap();
    (tmp, store, build)
}

fn chunks(store: &ProjectStore, build: &str) -> Vec<ChunkRow> {
    let file = store.files(build).unwrap().remove(0);
    store.file_chunks(build, &file.id).unwrap()
}

fn texts(v: &serde_json::Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|p| p["text"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn heading_siblings_tables_and_budget() {
    let (_tmp, store, build) = index();
    let all = chunks(&store, &build);
    // 0 Guide; 1 intro (under Guide); 2 Setup (under Guide); 3 the merged
    // steps paragraph and 4 the table (under Setup); 5 Usage; 6 its text.
    let by = |ord: i64| all.iter().find(|c| c.ordinal == ord).unwrap();
    assert_eq!(by(3).parent_ordinal, Some(2));
    let opts = ContextOptions {
        parent_heading: true,
        previous_chunks: 1,
        next_chunks: 1,
        max_tokens: 1200,
    };
    let c = expand_chunk_context(&store, &build, &by(3).id, &opts)
        .unwrap()
        .unwrap();
    assert!(c["matched"]["text"]
        .as_str()
        .unwrap()
        .starts_with("Step one text."));
    assert_eq!(c["parent_heading"]["text"], "Setup");
    assert!(
        texts(&c["previous"]).is_empty(),
        "first child of its heading"
    );
    let next = texts(&c["next"]);
    assert_eq!(next.len(), 1);
    assert!(
        next[0].contains("Key | Value") && next[0].contains("a | 1"),
        "{next:?}"
    );
    assert_eq!(c["truncated"], false);

    // Siblings stop at the heading's own children.
    let wide = ContextOptions {
        previous_chunks: 5,
        next_chunks: 5,
        ..opts
    };
    let c = expand_chunk_context(&store, &build, &by(4).id, &wide)
        .unwrap()
        .unwrap();
    assert_eq!(c["matched"]["kind"], "table");
    assert_eq!(texts(&c["previous"]).len(), 1);
    assert!(texts(&c["next"]).is_empty());

    // A top-level heading's siblings are the other top-level chunks.
    let c = expand_chunk_context(&store, &build, &by(0).id, &wide)
        .unwrap()
        .unwrap();
    assert!(c["parent_heading"].is_null());

    // A tight budget keeps the match, drops what overflows, and says why.
    let tight = ContextOptions {
        max_tokens: 5,
        ..wide
    };
    let c = expand_chunk_context(&store, &build, &by(3).id, &tight)
        .unwrap()
        .unwrap();
    assert!(c["matched"]["text"]
        .as_str()
        .unwrap()
        .starts_with("Step one"));
    assert_eq!(c["truncated"], true);
    assert!(c["parent_heading"].is_null());
    assert!(c["truncation_reasons"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r.as_str().unwrap().contains("max_tokens=5 reached")));

    // Not a chunk: no context.
    assert!(expand_chunk_context(&store, &build, "nope", &opts)
        .unwrap()
        .is_none());
    let off = ContextOptions {
        parent_heading: false,
        previous_chunks: 0,
        next_chunks: 0,
        max_tokens: 1200,
    };
    assert!(off.disabled());
}
