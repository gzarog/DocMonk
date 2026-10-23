//! Unicode and oversized-content boundary behavior.

use docmonk_config::model::{ChunkingConfig, MaxTokens};
use docmonk_documents::chunker::chunk_document;
use docmonk_documents::model::{DocumentMetadata, NormalizedDocument, NormalizedUnit, UnitKind};
use docmonk_documents::tokenization::{count_tokens, split_by_token_budget};
use docmonk_documents::tokenizer::model_tokenizer;
use docmonk_documents::version::document_version_stamp;

fn para(text: &str) -> NormalizedUnit {
    NormalizedUnit {
        kind: UnitKind::Paragraph,
        text: text.into(),
        heading_level: None,
        heading_path: vec![],
        parent_index: None,
        page_start: None,
        page_end: None,
        table_rows: None,
        caption: None,
        header_row_count: 0,
    }
}

#[test]
fn splits_never_cut_inside_a_character_and_cover_the_text() {
    let tok = model_tokenizer().unwrap();
    let text = "Ελληνικά 日本語 🚀🔥👍🏽 e\u{301} ﬁ".repeat(40);
    for budget in [1, 2, 5, 13] {
        let pieces = tok.split(&text, budget);
        assert!(!pieces.is_empty());
        for p in &pieces {
            // Every piece is a valid &str slice of the original text.
            assert!(text.contains(p.as_str()));
        }
        for p in split_by_token_budget(&text, budget) {
            assert!(
                count_tokens(&p) <= budget || p.split_whitespace().count() == 1,
                "{p:?}"
            );
        }
    }
}

#[test]
fn oversized_content_is_bounded_and_deterministic() {
    let huge_word = "x".repeat(20_000);
    let big = format!("{} {}", "word ".repeat(5_000), huge_word);
    let doc = NormalizedDocument {
        title: Some("Big".into()),
        page_count: None,
        is_scanned: false,
        units: vec![para(&big)],
    };
    let cfg = ChunkingConfig {
        max_tokens: MaxTokens::Value(64),
        min_tokens: 8,
        overlap_tokens: 4,
        ..ChunkingConfig::default()
    };
    let a = chunk_document(&doc, &cfg, "Big", None);
    let b = chunk_document(&doc, &cfg, "Big", None);
    assert_eq!(a, b);
    let tok = model_tokenizer().unwrap();
    let ceiling = cfg.resolved_max_tokens() - cfg.safety_tokens;
    for c in &a {
        assert!(tok.count(&c.contextual_text, true) <= ceiling);
    }
    assert!(a.len() > 50);
}

#[test]
fn empty_and_whitespace_documents() {
    let doc = NormalizedDocument::default();
    assert!(chunk_document(&doc, &ChunkingConfig::default(), "", None).is_empty());
    assert_eq!(count_tokens(""), 0);
    assert!(split_by_token_budget("", 4).is_empty());
}

#[test]
fn metadata_and_version_stamp() {
    let doc = NormalizedDocument::default();
    let m = DocumentMetadata::from_normalized(&doc, "markdown", "notes.draft.md");
    assert_eq!(m.title.as_deref(), Some("notes.draft"));
    assert_eq!(m.author, None);
    let s = document_version_stamp("1", true);
    assert_eq!(s.parser_version, "1+eml-attachments.1");
    assert!(s.chunker_version.starts_with("3+tok:"));
}
