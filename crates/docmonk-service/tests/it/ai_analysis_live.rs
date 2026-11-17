//! Live AI file analysis against real accounts (ADR 0045), compared with the
//! native extraction on the document fixtures. Ignored by default: it sends
//! the fixtures to the providers and spends their allowance.
//!
//! ```sh
//! DOCMONK_AI_ANALYSIS_LIVE=1 DOCMONK_AI_ANALYSIS_PROVIDERS=anthropic,claude_code \
//! DOCMONK_EVAL_OUT=benchmarks/ai-analysis/live.json \
//! cargo test --release -p docmonk-service --test it -- ai_analysis_live:: --ignored
//! ```
//!
//! Per provider and file it records the chunk count, pages covered, the
//! share of native words the analysis kept (recall, a measure, never a
//! gate), the failure code if any, and the wall time.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Instant;

use docmonk_convert::converter::{DoclingConverter, DocumentConverter};
use docmonk_convert::format::detect_format;
use docmonk_convert::normalize::normalize;
use serde_json::{json, Value};

const FILES: &[&str] = &[
    "handbook.md",
    "simple.html",
    "document.docx",
    "presentation.pptx",
    "spreadsheet.xlsx",
    "sample.pdf",
    "five_pages.pdf",
    "scanned.pdf",
    "sample_ocr.png",
];

fn words(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() > 2)
        .map(str::to_lowercase)
        .collect()
}

#[test]
#[ignore = "sends files to real AI accounts; set DOCMONK_AI_ANALYSIS_LIVE=1"]
fn live_providers_against_native_extraction() {
    if std::env::var_os("DOCMONK_AI_ANALYSIS_LIVE").is_none() {
        eprintln!("DOCMONK_AI_ANALYSIS_LIVE is not set; nothing sent");
        return;
    }
    let providers =
        std::env::var("DOCMONK_AI_ANALYSIS_PROVIDERS").unwrap_or_else(|_| "anthropic".into());
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/documents");
    let mut report = Vec::new();
    for provider in providers
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        let mut cfg = docmonk_config::DocMonkConfig::default();
        cfg.privacy.external_ai_allowed = true;
        cfg.documents.analysis.mode = "ai".into();
        cfg.documents.analysis.provider = provider.into();
        if let Ok(m) = std::env::var(format!(
            "DOCMONK_AI_ANALYSIS_MODEL_{}",
            provider.to_uppercase()
        )) {
            cfg.documents.analysis.model = m;
        }
        let mut files = Vec::new();
        for name in FILES {
            let path = root.join(name);
            let fmt = detect_format(&path).unwrap();
            let native = DoclingConverter::default()
                .convert(&path, fmt)
                .ok()
                .map(|j| normalize(&j, fmt.as_str() == "pdf"));
            let native_words: BTreeSet<String> = native
                .iter()
                .flat_map(|d| d.units.iter().map(|u| u.text.clone()))
                .flat_map(|t| words(&t))
                .collect();
            let started = Instant::now();
            let outcome = docmonk_convert::ai::preview(&cfg, &path);
            let secs = started.elapsed().as_secs_f64();
            let entry = match outcome {
                Ok((extracted_by, r)) => {
                    let ai_words: BTreeSet<String> =
                        r.chunks.iter().flat_map(|c| words(&c.text)).collect();
                    let recall = if native_words.is_empty() {
                        Value::Null
                    } else {
                        json!(
                            native_words.intersection(&ai_words).count() as f64
                                / native_words.len() as f64
                        )
                    };
                    let pages: BTreeSet<i64> = r
                        .chunks
                        .iter()
                        .flat_map(|c| [c.page_start, c.page_end])
                        .flatten()
                        .collect();
                    json!({"file": name, "extracted_by": extracted_by, "chunks": r.chunks.len(),
                           "pages_covered": pages.len(), "native_word_recall": recall,
                           "code_references": r.code_references.len(), "seconds": secs})
                }
                Err(e) => json!({"file": name, "error": e.message(), "seconds": secs}),
            };
            eprintln!("{provider}: {entry}");
            files.push(entry);
        }
        report.push(json!({"provider": provider, "files": files}));
    }
    let out = json!({"schema": "docmonk-ai-analysis-live", "providers": report});
    if let Ok(p) = std::env::var("DOCMONK_EVAL_OUT") {
        std::fs::write(&p, serde_json::to_string_pretty(&out).unwrap() + "\n").unwrap();
    }
}
