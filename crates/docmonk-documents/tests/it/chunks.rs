//! Chunking against fixtures/expected/documents.json: every normalized
//! document (TXT/Markdown/HTML fixtures plus synthetic table/heading/Unicode
//! edge cases) under every configuration listed there.

use std::path::Path;

use docmonk_config::model::{ChunkingConfig, MaxTokens};
use docmonk_documents::chunker::{
    chunk_document, chunker_version_stamp, ChunkingDiagnostics, EMBEDDING_TEXT_VERSION,
};
use docmonk_documents::model::NormalizedDocument;
use docmonk_documents::tokenizer::model_tokenizer;
use serde_json::Value;

fn golden() -> Value {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/expected/documents.json");
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn config(v: &Value) -> ChunkingConfig {
    let mut c = ChunkingConfig::default();
    if let Some(n) = v.get("max_tokens").and_then(Value::as_i64) {
        c.max_tokens = MaxTokens::Value(n);
    }
    if let Some(n) = v.get("min_tokens").and_then(Value::as_i64) {
        c.min_tokens = n;
    }
    if let Some(n) = v.get("overlap_tokens").and_then(Value::as_i64) {
        c.overlap_tokens = n;
    }
    if let Some(n) = v.get("safety_tokens").and_then(Value::as_i64) {
        c.safety_tokens = n;
    }
    if let Some(b) = v.get("merge_peers").and_then(Value::as_bool) {
        c.merge_peers = b;
    }
    c
}

#[test]
fn version_stamp_is_as_expected() {
    let g = golden();
    assert_eq!(g["identity"]["chunker_version"], chunker_version_stamp());
    assert_eq!(
        g["identity"]["embedding_text_version"],
        EMBEDDING_TEXT_VERSION
    );
}

#[test]
fn chunks_are_as_expected_for_every_document_and_config() {
    let g = golden();
    let tok = model_tokenizer().unwrap();
    let mut failures = Vec::new();
    let mut checked = 0;
    for (name, entry) in g["documents"].as_object().unwrap() {
        let doc: NormalizedDocument = serde_json::from_value(entry["normalized"].clone())
            .unwrap_or_else(|e| panic!("{name}: canonical JSON does not parse: {e}"));
        // The canonical format round-trips losslessly.
        assert_eq!(
            serde_json::to_value(&doc).unwrap(),
            entry["normalized"],
            "{name}"
        );
        let title = entry["doc_title"].as_str().unwrap();
        for (cfg_name, want) in entry["chunks"].as_object().unwrap() {
            let cfg = config(&g["configs"][cfg_name]);
            let mut diag = ChunkingDiagnostics::default();
            let chunks = chunk_document(&doc, &cfg, title, Some(&mut diag));
            let got = serde_json::to_value(&chunks).unwrap();
            checked += 1;
            if got != want["chunks"] {
                let gw = want["chunks"].as_array().unwrap();
                let first = chunks
                    .iter()
                    .zip(gw)
                    .position(|(c, w)| serde_json::to_value(c).unwrap() != *w);
                failures.push(format!(
                    "{name}/{cfg_name}: {} vs {} chunks; first diff at {first:?}:\n got      {}\n expected {}",
                    chunks.len(),
                    gw.len(),
                    first.map(|i| serde_json::to_string(&chunks[i]).unwrap()).unwrap_or_default(),
                    first.map(|i| gw[i].to_string()).unwrap_or_default(),
                ));
                continue;
            }
            if serde_json::to_value(&diag).unwrap() != want["diagnostics"] {
                failures.push(format!(
                    "{name}/{cfg_name}: diagnostics {diag:?} vs {}",
                    want["diagnostics"]
                ));
            }
            // The payload ceiling holds for every non-atomic chunk (headings
            // are stored whole by design).
            let ceiling = cfg.resolved_max_tokens() - cfg.safety_tokens;
            for c in &chunks {
                let n = tok.count(&c.contextual_text, true);
                let atomic_cell = c.caption.as_deref().is_some_and(|cap| cap.contains("Row "));
                let heading = c.kind == docmonk_documents::model::UnitKind::Heading;
                if n > ceiling && !atomic_cell && !heading {
                    failures.push(format!("{name}/{cfg_name}: payload {n} > {ceiling}"));
                }
            }
        }
    }
    assert_eq!(checked, 30);
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
