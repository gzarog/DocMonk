//! Native document processing through the indexer.

use crate::common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use docmonk_core::paths::project_id_for_canonical;
use docmonk_indexing::coordinator::{run_source, NoProgress, Options};
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::StorageLayout;
use serde_json::Value;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn documents_are_converted_chunked_and_searchable() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("docs");
    std::fs::create_dir_all(&root).unwrap();
    for e in std::fs::read_dir(repo_root().join("fixtures/documents")).unwrap() {
        let p = e.unwrap().path();
        let bytes = std::fs::read(&p).unwrap();
        // Text fixtures are normalized to LF so chunks match the golden.
        let bytes = match p.extension().and_then(|x| x.to_str()) {
            Some("md" | "txt" | "html" | "csv") => String::from_utf8(bytes)
                .unwrap()
                .replace("\r\n", "\n")
                .into_bytes(),
            _ => bytes,
        };
        std::fs::write(root.join(p.file_name().unwrap()), bytes).unwrap();
    }

    let home = common::home(tmp.path());
    let layout = StorageLayout::new(&home);
    let mut cp = common::control(&home);
    let src = common::add_source(&mut cp, &root, &[], &[]);
    let cfg = docmonk_config::DocMonkConfig::default();
    let reg = docmonk_convert::registry(&cfg);
    let mut opts = Options::from_config(&cfg);
    opts.workers = 4;
    let r = run_source(&layout, &mut cp, &src, &reg, &opts, &mut NoProgress).unwrap();
    assert!(r.published, "{r:?}");
    assert_eq!(r.failed, 1, "only corrupt.docx fails");

    let active = cp.state(&src.id).unwrap().active_build_id.unwrap();
    let store =
        ProjectStore::open(&layout, &project_id_for_canonical(&src.path), &src.id, 8).unwrap();
    let files: HashMap<String, _> = store
        .files(&active)
        .unwrap()
        .into_iter()
        .map(|f| (f.rel_path.clone(), f))
        .collect();
    assert_eq!(files["corrupt.docx"].status, "failed");
    assert_eq!(files["sample.pdf"].status, "indexed");
    assert_eq!(
        files["sample_ocr.png"].status, "indexed",
        "image_ocr off: no content, not failed"
    );

    // Stored chunks equal the expected chunks (default config).
    let golden: Value = serde_json::from_str(
        &std::fs::read_to_string(repo_root().join("fixtures/expected/documents.json")).unwrap(),
    )
    .unwrap();
    let mut compared = 0;
    for (name, entry) in golden["documents"].as_object().unwrap() {
        let Some(f) = files.get(name) else { continue };
        let want = entry["chunks"]["default"]["chunks"].as_array().unwrap();
        let got = store.file_chunks(&active, &f.id).unwrap();
        assert_eq!(got.len(), want.len(), "{name}");
        for (g, w) in got.iter().zip(want) {
            assert_eq!(g.kind, w["kind"].as_str().unwrap(), "{name}");
            assert_eq!(g.text, w["text"].as_str().unwrap(), "{name}");
            assert_eq!(g.search_text, w["search_text"].as_str().unwrap(), "{name}");
            assert_eq!(
                g.embedding_text.as_deref(),
                w["contextual_text"].as_str(),
                "{name}"
            );
            assert_eq!(g.token_count, w["token_count"].as_i64(), "{name}");
            assert_eq!(
                serde_json::to_value(&g.heading_path).unwrap(),
                w["heading_path"],
                "{name}"
            );
            assert_eq!(
                serde_json::to_value(&g.table_rows).unwrap(),
                w["table_rows"],
                "{name}"
            );
        }
        compared += 1;
    }
    assert_eq!(compared, 6);

    let docs = store.documents(&active).unwrap();
    assert!(docs
        .iter()
        .any(|d| d.format == "docx" && d.title.as_deref() == Some("Doc Title")));
    let pdf = docs
        .iter()
        .find(|d| d.file_id == files["sample.pdf"].id)
        .expect("text-layer PDF converted");
    assert_eq!(
        (pdf.format.as_str(), pdf.page_count, pdf.is_scanned),
        ("pdf", Some(1), false)
    );
    let scan = docs
        .iter()
        .find(|d| d.file_id == files["scanned.pdf"].id)
        .unwrap();
    assert!(scan.is_scanned);
    assert!(!docs.iter().any(|d| d.file_id == files["sample_ocr.png"].id));
    assert!(!store
        .search_chunks(&active, "Sample PDF Title", 5)
        .unwrap()
        .is_empty());
    // Email attachments are child documents with provenance; the email
    // with a corrupt attachment is still indexed.
    let email = &files["email_with_attachments.eml"];
    let children: Vec<_> = docs
        .iter()
        .filter(|d| d.file_id == email.id && d.attachment.is_some())
        .collect();
    let mut names: Vec<_> = children
        .iter()
        .map(|d| d.attachment.as_ref().unwrap().name.clone().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "Résumé – memo.md",
            "budget.xlsx",
            "notes.txt",
            "report.docx"
        ]
    );
    assert_eq!(files["email_with_corrupt_attachment.eml"].status, "indexed");
    let hits = store.search_chunks(&active, "api-07", 5).unwrap();
    assert!(!hits.is_empty(), "table rows are searchable");

    // An edit re-chunks only that document, in place.
    std::fs::write(
        root.join("simple.md"),
        "# Changed\n\nNew unique zebracorn text.\n",
    )
    .unwrap();
    let inc = run_source(&layout, &mut cp, &src, &reg, &opts, &mut NoProgress).unwrap();
    assert_eq!((inc.counts.changed, inc.indexed), (1, 1));
    let store =
        ProjectStore::open(&layout, &project_id_for_canonical(&src.path), &src.id, 8).unwrap();
    assert_eq!(
        store.search_chunks(&active, "zebracorn", 5).unwrap().len(),
        1
    );
}
