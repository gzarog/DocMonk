//! AI file analysis as the document processor (ADR 0045), with a scripted
//! analyzer: the bypass, the input each format gets, caching, failure codes
//! without a native fallback, the chunk budget split, and the run-start
//! privacy check.

use crate::common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use docmonk_ai::analysis::{
    AiAnalyzer, AnalysisChunk, AnalysisError, AnalysisErrorKind, AnalysisInput, AnalysisResult,
    CodeReference, InputKind, InputPart,
};
use docmonk_convert::ai::{AiDocumentProcessor, Slots};
use docmonk_convert::cache::ConversionCache;
use docmonk_indexing::coordinator::{run_source, NoProgress, Options};
use docmonk_storage::StorageLayout;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn chunk(kind: &str, path: &[&str], text: &str) -> AnalysisChunk {
    AnalysisChunk {
        kind: kind.into(),
        heading_path: path.iter().map(|s| s.to_string()).collect(),
        heading_level: (kind == "heading").then_some(path.len() as i64 + 1),
        page_start: None,
        page_end: None,
        text: text.into(),
        embedding_text: None,
        table_rows: (kind == "table").then(|| vec![vec!["a".into(), "b".into()]]),
    }
}

fn result(chunks: Vec<AnalysisChunk>) -> AnalysisResult {
    AnalysisResult {
        schema_version: 1,
        title: Some("Operations Handbook".into()),
        author: Some("Ops".into()),
        language: Some("en".into()),
        summary: Some("How operations run.".into()),
        keywords: vec!["operations".into()],
        page_count: None,
        chunks,
        code_references: vec![],
    }
}

/// Returns `reply` for every file and records what it was sent.
struct Scripted {
    reply: Mutex<Result<AnalysisResult, AnalysisError>>,
    calls: AtomicUsize,
    seen: Mutex<Vec<AnalysisInput>>,
    text_only: bool,
}

impl Scripted {
    fn new(reply: Result<AnalysisResult, AnalysisError>) -> Arc<Self> {
        Arc::new(Self {
            reply: Mutex::new(reply),
            calls: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
            text_only: false,
        })
    }
}

impl AiAnalyzer for Scripted {
    fn provider_id(&self) -> &'static str {
        "scripted"
    }
    fn model(&self) -> &str {
        "m1"
    }
    fn accepts(&self, kind: InputKind) -> bool {
        !self.text_only || kind == InputKind::Text
    }
    fn analyze(&self, input: &AnalysisInput) -> Result<AnalysisResult, AnalysisError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(input.clone());
        self.reply.lock().unwrap().clone()
    }
}

fn processor(a: Arc<Scripted>, cache: Option<&Path>) -> AiDocumentProcessor {
    AiDocumentProcessor {
        analyzer: a,
        chunking: Default::default(),
        max_file_bytes: 1024 * 1024,
        max_pages: Some(1000),
        attachments: None,
        cache: cache.map(ConversionCache::new),
        slots: Slots::new(2),
    }
}

#[test]
fn text_files_are_sent_as_text_and_mapped_to_rows() {
    let a = Scripted::new(Ok(result(vec![
        chunk("heading", &[], "Operations Handbook"),
        chunk("text", &["Operations Handbook"], "Call docmonk index."),
        chunk("table", &["Operations Handbook"], "a b"),
    ])));
    let p = processor(a.clone(), None);
    let path = repo_root().join("fixtures/documents/handbook.md");
    let k = p.knowledge(&path, "f1", Some("h")).unwrap();
    let seen = a.seen.lock().unwrap();
    assert_eq!(seen[0].format, "markdown");
    let InputPart::Text { text, .. } = &seen[0].parts[0] else {
        panic!("markdown must go as text")
    };
    assert!(text.contains("supercalifragilisticexpialidocious"));
    assert_eq!(k.documents.len(), 1);
    let d = &k.documents[0];
    assert_eq!(d.title.as_deref(), Some("Operations Handbook"));
    assert_eq!(d.author.as_deref(), Some("Ops"));
    assert_eq!(d.language.as_deref(), Some("en"));
    let kinds: Vec<_> = k.chunks.iter().map(|c| c.kind.as_str()).collect();
    assert_eq!(kinds, ["heading", "paragraph", "table"]);
    assert_eq!(k.chunks[1].text, "Call docmonk index.");
    assert_eq!(k.chunks[1].parent_ordinal, Some(0));
    let e = k.chunks[1].embedding_text.as_deref().unwrap();
    assert!(
        e.starts_with("Document: Operations Handbook\nSummary: How operations run."),
        "{e}"
    );
    assert!(k.chunks[2].table_rows.is_some());
}

#[test]
fn pdfs_and_images_go_natively_and_containers_are_unzipped() {
    let a = Scripted::new(Ok(result(vec![chunk("text", &[], "x")])));
    let p = processor(a.clone(), None);
    for name in [
        "sample.pdf",
        "sample_ocr.png",
        "document.docx",
        "spreadsheet.ods",
    ] {
        p.knowledge(
            &repo_root().join("fixtures/documents").join(name),
            "f",
            None,
        )
        .unwrap();
    }
    let seen = a.seen.lock().unwrap();
    assert!(matches!(seen[0].parts[..], [InputPart::Pdf { .. }]));
    assert!(seen[0].page_count.is_some());
    assert!(
        matches!(&seen[1].parts[..], [InputPart::Image { media_type, .. }] if media_type == "image/png")
    );
    let names = |i: usize| -> Vec<String> {
        seen[i]
            .parts
            .iter()
            .map(|p| match p {
                InputPart::Text { name, .. } => name.clone().unwrap_or_default(),
                _ => panic!("containers go as text parts"),
            })
            .collect()
    };
    assert!(
        names(2).contains(&"word/document.xml".to_owned()),
        "{:?}",
        names(2)
    );
    assert!(!names(2)
        .iter()
        .any(|n| n.contains("styles") || n.ends_with(".rels")));
    assert_eq!(names(3), ["content.xml", "meta.xml"]);
}

#[test]
fn an_unchanged_file_is_analysed_once() {
    let dir = tempfile::tempdir().unwrap();
    let a = Scripted::new(Ok(result(vec![chunk("text", &[], "Hello")])));
    let path = repo_root().join("fixtures/documents/simple.md");
    let first = processor(a.clone(), Some(dir.path()))
        .knowledge(&path, "f", None)
        .unwrap();
    let again = processor(a.clone(), Some(dir.path()))
        .knowledge(&path, "f", None)
        .unwrap();
    assert_eq!(a.calls.load(Ordering::SeqCst), 1);
    assert_eq!(first.chunks, again.chunks);
}

#[test]
fn failures_fail_the_file_with_their_code_and_no_rows() {
    let path = repo_root().join("fixtures/documents/simple.md");
    for (kind, code, transient) in [
        (AnalysisErrorKind::Quota, "ai_analysis_quota", true),
        (
            AnalysisErrorKind::NotSignedIn,
            "ai_analysis_not_signed_in",
            true,
        ),
        (
            AnalysisErrorKind::InvalidOutput,
            "ai_analysis_invalid_output",
            false,
        ),
        (AnalysisErrorKind::Refused, "ai_analysis_refused", false),
    ] {
        let a = Scripted::new(Err(AnalysisError::new(kind, "nope")));
        let e = processor(a, None).knowledge(&path, "f", None).unwrap_err();
        assert_eq!(e.code, code);
        assert_eq!(e.transient, transient, "{code}");
    }
    let a = Scripted::new(Ok(result(vec![chunk("text", &[], "x")])));
    let mut p = processor(a.clone(), None);
    p.max_file_bytes = 10;
    let e = p.knowledge(&path, "f", None).unwrap_err();
    assert_eq!(e.code, "ai_analysis_too_large");
    assert_eq!(a.calls.load(Ordering::SeqCst), 0, "never sent");
    let text_only = Arc::new(Scripted {
        reply: Mutex::new(Ok(result(vec![chunk("text", &[], "x")]))),
        calls: AtomicUsize::new(0),
        seen: Mutex::new(Vec::new()),
        text_only: true,
    });
    let e = processor(text_only.clone(), None)
        .knowledge(
            &repo_root().join("fixtures/documents/sample.pdf"),
            "f",
            None,
        )
        .unwrap_err();
    assert_eq!(e.code, "ai_analysis_unsupported_format");
    assert_eq!(text_only.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn an_oversized_chunk_is_split_within_the_budget_and_references_are_verified() {
    let long = "The settlement retry service runs every hour and records each attempt. ".repeat(80);
    let mut r = result(vec![chunk("text", &["Retries"], &long)]);
    r.code_references = vec![
        CodeReference {
            kind: "symbol".into(),
            value: "settlement".into(),
            chunk_index: Some(0),
            quote: Some("settlement retry".into()),
        },
        CodeReference {
            kind: "symbol".into(),
            value: "GhostService".into(),
            chunk_index: Some(0),
            quote: None,
        },
    ];
    let a = Scripted::new(Ok(r));
    let p = processor(a, None);
    let k = p
        .knowledge(&repo_root().join("fixtures/documents/simple.md"), "f", None)
        .unwrap();
    assert!(k.chunks.len() > 1, "split");
    let tok = docmonk_documents::tokenizer::model_tokenizer().unwrap();
    let cfg = docmonk_config::model::ChunkingConfig::default();
    let budget = cfg.resolved_max_tokens() - cfg.safety_tokens;
    for c in &k.chunks {
        assert!(tok.count(c.embedding_text.as_deref().unwrap(), true) <= budget);
        let e = c.embedding_text.as_deref().unwrap();
        assert!(
            e.contains("Mentions: settlement") && !e.contains("GhostService"),
            "{e}"
        );
    }
    let rejoined: String = k
        .chunks
        .iter()
        .map(|c| c.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(
        rejoined.split_whitespace().count(),
        long.split_whitespace().count()
    );
}

#[test]
fn ai_mode_without_consent_stops_before_any_file() {
    let mut cfg = docmonk_config::DocMonkConfig::default();
    cfg.documents.analysis.mode = "ai".into();
    cfg.documents.analysis.provider = "claude_code".into();
    let e = docmonk_convert::check_analysis(&cfg).unwrap_err();
    assert_eq!(e.exit_code(), 8);
    // A registry built anyway fails document files; it never falls back.
    let reg = docmonk_convert::registry(&cfg);
    assert!(reg
        .versions
        .converter_version
        .starts_with("ai-analysis.1+unavailable"));
    cfg.privacy.external_ai_allowed = true;
    docmonk_convert::check_analysis(&cfg).unwrap();
    let reg = docmonk_convert::registry(&cfg);
    assert!(
        reg.versions
            .converter_version
            .starts_with("ai-analysis.1+claude_code+default+schema.1+prompt."),
        "{}",
        reg.versions.converter_version
    );
    // Native mode keeps its identity.
    let native = docmonk_convert::registry(&docmonk_config::DocMonkConfig::default());
    assert!(native
        .versions
        .converter_version
        .starts_with("docling.rs-2.67.1"));
}

#[test]
fn an_index_run_publishes_ai_rows_and_counts_failed_files() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("docs");
    common::write(&root, "ok.md", "# Hello\n\nWorld\n");
    common::write(&root, "code.py", "def hello():\n    return 1\n");
    let home = common::home(tmp.path());
    let layout = StorageLayout::new(&home);
    let mut cp = common::control(&home);
    let src = common::add_source(&mut cp, &root, &[], &[]);
    let cfg = docmonk_config::DocMonkConfig::default();
    let mut reg = docmonk_convert::registry(&cfg);
    let a = Scripted::new(Ok(result(vec![chunk("text", &[], "World")])));
    reg.document = Arc::new(processor(a.clone(), None));
    let r = run_source(
        &layout,
        &mut cp,
        &src,
        &reg,
        &Options::from_config(&cfg),
        &mut NoProgress,
    )
    .unwrap();
    assert!(r.published, "{r:?}");
    assert_eq!(r.failed, 0);
    assert_eq!(
        a.calls.load(Ordering::SeqCst),
        1,
        "only the document goes to the model"
    );

    common::write(&root, "bad.md", "# Bad\n");
    let bad = Scripted::new(Err(AnalysisError::new(
        AnalysisErrorKind::InvalidOutput,
        "garbage",
    )));
    reg.document = Arc::new(processor(bad, None));
    let r = run_source(
        &layout,
        &mut cp,
        &src,
        &reg,
        &Options::from_config(&cfg),
        &mut NoProgress,
    )
    .unwrap();
    assert!(r.failed >= 1, "{r:?}");
}
