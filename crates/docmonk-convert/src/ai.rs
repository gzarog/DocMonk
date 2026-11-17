//! AI file analysis as the document processor (ADR 0045).
//!
//! With `documents.analysis.mode: ai`, every document file goes to the
//! configured [`AiAnalyzer`] instead of the embedded extraction: docling.rs,
//! the PDF text layer, OCR, the normalizer, the chunker and
//! `DocumentMetadata` do not run. The model returns the chunks, headings,
//! pages, tables, title, author, language, summary, keywords and code
//! references; this module only
//!
//! * builds the model input: PDFs and images as they are, text formats as
//!   text, and Office/OpenDocument/EPUB containers unzipped into their XML
//!   and HTML parts (decision D1: opening the zip is not analysis);
//! * enforces `documents.analysis.max_file_bytes` and `documents.max_pages`;
//! * caches every validated result by content hash and analysis identity,
//!   so an unchanged file is never sent twice;
//! * maps the result to rows, splitting a chunk only when it exceeds the
//!   embedding budget (decision D3);
//! * records a failure with its `ai_analysis_*` code. There is no fallback
//!   to the embedded extraction: a failed file has no document rows.

use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};

use docmonk_ai::analysis::{
    AiAnalyzer, AnalysisError, AnalysisErrorKind, AnalysisInput, AnalysisResult, InputPart,
};
use docmonk_config::model::ChunkingConfig;
use docmonk_core::ids::record;
use docmonk_core::lexnorm::{detect_language, detect_script};
use docmonk_documents::tokenization::split_by_token_budget;
use docmonk_documents::tokenizer::model_tokenizer;
use docmonk_indexing::coordinator::{PrepareInput, ProcessError, Processor, SKIPPED_LIMIT};
use docmonk_storage::knowledge::{AttachmentProvenance, ChunkRow, DocumentRow, FileKnowledge};
use sha2::{Digest, Sha256};

use crate::cache::ConversionCache;
use crate::email::{self, AttachmentSettings};
use crate::format::{detect_format, DocumentFormat};

/// The analysis path's converter identity prefix.
pub const AI_ANALYSIS_VERSION: &str = "ai-analysis.1";

/// The converter identity of AI analysis: a change of provider, model,
/// prompt or schema rebuilds every document.
pub fn converter_identity(analyzer: &dyn AiAnalyzer) -> String {
    format!(
        "{AI_ANALYSIS_VERSION}+{}+{}+{}",
        analyzer.provider_id(),
        analyzer.model(),
        docmonk_ai::analysis::analysis_identity()
    )
}

/// Bounds the analyses in flight (`documents.analysis.concurrency`).
pub struct Slots {
    free: Mutex<usize>,
    cv: Condvar,
}

impl Slots {
    pub fn new(n: usize) -> Self {
        Self {
            free: Mutex::new(n.max(1)),
            cv: Condvar::new(),
        }
    }

    fn run<T>(&self, f: impl FnOnce() -> T) -> T {
        let mut free = self.free.lock().unwrap_or_else(|p| p.into_inner());
        while *free == 0 {
            free = self.cv.wait(free).unwrap_or_else(|p| p.into_inner());
        }
        *free -= 1;
        drop(free);
        struct Release<'a>(&'a Slots);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                *self.0.free.lock().unwrap_or_else(|p| p.into_inner()) += 1;
                self.0.cv.notify_one();
            }
        }
        let _r = Release(self);
        f()
    }
}

pub struct AiDocumentProcessor {
    pub analyzer: Arc<dyn AiAnalyzer>,
    pub chunking: ChunkingConfig,
    /// `documents.analysis.max_file_bytes`.
    pub max_file_bytes: u64,
    /// `documents.max_pages`: larger PDFs are recorded `skipped_limit`.
    pub max_pages: Option<usize>,
    /// `documents.email_attachment*`; `None` disables attachments.
    pub attachments: Option<AttachmentSettings>,
    pub cache: Option<ConversionCache>,
    pub slots: Slots,
}

fn error(path: &Path, e: AnalysisError) -> ProcessError {
    ProcessError {
        code: e.kind.code().into(),
        message: format!("{}: {}", path.display(), e.message),
        transient: e.kind.transient(),
    }
}

fn too_large(path: &Path, size: u64, max: u64) -> ProcessError {
    error(
        path,
        AnalysisError::new(
            AnalysisErrorKind::TooLarge,
            format!("{size} bytes exceeds documents.analysis.max_file_bytes ({max})"),
        ),
    )
}

/// The image media type of a file name, when the providers take it.
fn image_media_type(name: &str) -> Option<&'static str> {
    let lower = name.to_lowercase();
    if lower.ends_with(".png") {
        Some("image/png")
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        Some("image/jpeg")
    } else {
        None
    }
}

/// Container members that carry content (not styles, themes, settings or
/// relationships), in archive order.
fn content_member(format: DocumentFormat, name: &str) -> bool {
    let lower = name.to_lowercase();
    let markup = lower.ends_with(".xml")
        || lower.ends_with(".xhtml")
        || lower.ends_with(".html")
        || lower.ends_with(".htm");
    if !markup || lower.ends_with(".rels") || lower.contains("_rels/") {
        return false;
    }
    const NOISE: &[&str] = &[
        "[content_types]",
        "styles",
        "theme",
        "settings",
        "fonttable",
        "websettings",
        "numbering",
        "app.xml",
        "manifest.xml",
        "presprops",
        "viewprops",
        "tablestyles",
        "slidemaster",
        "slidelayout",
        "notesmaster",
        "handoutmaster",
        "calcchain",
        "meta-inf/container.xml",
    ];
    if NOISE.iter().any(|n| lower.contains(n)) {
        return false;
    }
    match format {
        DocumentFormat::Docx => lower.starts_with("word/") || lower == "docprops/core.xml",
        DocumentFormat::Pptx => {
            lower.starts_with("ppt/slides/")
                || lower.starts_with("ppt/notesslides/")
                || lower == "docprops/core.xml"
        }
        DocumentFormat::Xlsx => {
            lower.starts_with("xl/worksheets/")
                || lower == "xl/sharedstrings.xml"
                || lower == "xl/workbook.xml"
                || lower == "docprops/core.xml"
        }
        DocumentFormat::Odt | DocumentFormat::Ods | DocumentFormat::Odp => {
            lower == "content.xml" || lower == "meta.xml"
        }
        DocumentFormat::Epub => true,
        _ => false,
    }
}

/// Unzips a container into named text parts, reading at most `budget`
/// uncompressed bytes in total.
fn container_parts(
    bytes: &[u8],
    format: DocumentFormat,
    budget: u64,
) -> Result<Vec<InputPart>, AnalysisError> {
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| {
        AnalysisError::new(
            AnalysisErrorKind::Rejected,
            format!("not a readable {} container: {e}", format.as_str()),
        )
    })?;
    let mut parts = Vec::new();
    let mut used = 0u64;
    for i in 0..z.len() {
        let mut f = z
            .by_index(i)
            .map_err(|e| AnalysisError::new(AnalysisErrorKind::Rejected, e.to_string()))?;
        let name = f.name().map(|n| n.into_owned()).unwrap_or_default();
        if f.is_dir() || !content_member(format, &name) {
            continue;
        }
        let left = budget.saturating_sub(used);
        let mut buf = Vec::new();
        // Read one byte past the budget to detect an oversized member.
        (&mut f)
            .take(left + 1)
            .read_to_end(&mut buf)
            .map_err(|e| AnalysisError::new(AnalysisErrorKind::Rejected, e.to_string()))?;
        if buf.len() as u64 > left {
            return Err(AnalysisError::new(
                AnalysisErrorKind::TooLarge,
                format!(
                    "the unpacked {} content exceeds documents.analysis.max_file_bytes ({budget})",
                    format.as_str()
                ),
            ));
        }
        used += buf.len() as u64;
        parts.push(InputPart::Text {
            name: Some(name),
            text: String::from_utf8_lossy(&buf).into_owned(),
        });
    }
    if parts.is_empty() {
        return Err(AnalysisError::new(
            AnalysisErrorKind::UnsupportedFormat,
            format!(
                "no content parts found in the {} container",
                format.as_str()
            ),
        ));
    }
    Ok(parts)
}

/// The model input for one file's bytes.
pub fn input_for(
    file_name: &str,
    format: DocumentFormat,
    bytes: Vec<u8>,
    max_file_bytes: u64,
    page_count: Option<i64>,
) -> Result<AnalysisInput, AnalysisError> {
    let parts = match format {
        DocumentFormat::Pdf => vec![InputPart::Pdf { bytes }],
        DocumentFormat::Image => match image_media_type(file_name) {
            Some(mt) => vec![InputPart::Image {
                media_type: mt.into(),
                bytes,
            }],
            None => {
                return Err(AnalysisError::new(
                    AnalysisErrorKind::UnsupportedFormat,
                    "only PNG and JPEG images can be analysed",
                ))
            }
        },
        DocumentFormat::Markdown
        | DocumentFormat::Txt
        | DocumentFormat::Html
        | DocumentFormat::Csv
        | DocumentFormat::Eml => {
            vec![InputPart::Text {
                name: None,
                text: String::from_utf8_lossy(&bytes).into_owned(),
            }]
        }
        DocumentFormat::Docx
        | DocumentFormat::Pptx
        | DocumentFormat::Xlsx
        | DocumentFormat::Odt
        | DocumentFormat::Ods
        | DocumentFormat::Odp
        | DocumentFormat::Epub => container_parts(&bytes, format, max_file_bytes)?,
    };
    Ok(AnalysisInput {
        file_name: file_name.to_owned(),
        format: format.as_str().to_owned(),
        parts,
        page_count,
    })
}

/// The stored kind of a model chunk kind.
fn stored_kind(kind: &str) -> &'static str {
    match kind {
        "heading" => "heading",
        "table" => "table",
        _ => "paragraph",
    }
}

/// Who a document's rows belong to.
pub struct DocumentTarget<'a> {
    pub file_id: &'a str,
    pub document_id: String,
    pub format: DocumentFormat,
    /// The title when the model gives none (the file stem).
    pub fallback_title: &'a str,
    /// Pages counted from the file itself, when it is paged.
    pub page_count: Option<i64>,
    pub content_hash: Option<String>,
    pub attachment: Option<AttachmentProvenance>,
}

/// Rows of one analysed document.
pub fn rows(
    result: &AnalysisResult,
    chunking: &ChunkingConfig,
    target: DocumentTarget<'_>,
) -> (DocumentRow, Vec<ChunkRow>) {
    let DocumentTarget {
        file_id,
        document_id,
        format,
        fallback_title,
        page_count,
        content_hash,
        attachment,
    } = target;
    let tok = model_tokenizer().expect("bundled tokenizer");
    let title = result
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| fallback_title.to_owned());
    // Document context every chunk's embedding text carries: title,
    // summary, keywords (paraphrase is allowed here, never in `text`).
    let mut context = vec![format!("Document: {title}")];
    if let Some(s) = result.summary.as_deref().filter(|s| !s.trim().is_empty()) {
        context.push(format!("Summary: {}", s.trim()));
    }
    if !result.keywords.is_empty() {
        context.push(format!("Keywords: {}", result.keywords.join(", ")));
    }
    let context = context.join("\n");
    let budget = (chunking.resolved_max_tokens() - chunking.safety_tokens).max(16);
    let mut out: Vec<ChunkRow> = Vec::new();
    // Ordinal of the last heading chunk at each depth (parents).
    let mut heading_at: Vec<Option<i64>> = Vec::new();
    for (ci, c) in result.chunks.iter().enumerate() {
        let kind = stored_kind(&c.kind);
        let depth = c.heading_path.len();
        let parent = depth
            .checked_sub(1)
            .and_then(|d| heading_at.get(d).copied().flatten());
        // Code names this chunk verifiably mentions (the quote must be in
        // the text); they help retrieval, the linker resolves them.
        let refs: Vec<&str> = result
            .code_references
            .iter()
            .filter(|r| r.chunk_index == Some(ci))
            .filter(|r| {
                r.quote.as_deref().is_none_or(|q| c.text.contains(q))
                    && c.text.contains(r.value.as_str())
            })
            .map(|r| r.value.as_str())
            .collect();
        let section = if c.heading_path.is_empty() {
            String::new()
        } else {
            format!("\nSection: {}", c.heading_path.join(" > "))
        };
        let mentions = if refs.is_empty() {
            String::new()
        } else {
            format!("\nMentions: {}", refs.join(", "))
        };
        let specials = tok.special_tokens();
        // The context header takes at most half the budget; the body gets
        // the rest, so the embedding input always fits.
        let mut header = format!("{context}{section}{mentions}");
        if tok.count(&header, false) > budget / 2 {
            header = tok
                .split(&header, budget / 2)
                .into_iter()
                .next()
                .unwrap_or_default();
        }
        let body_budget = (budget - specials - tok.count(&format!("{header}\n\n"), false)).max(8);
        let pieces = if kind == "table" || kind == "heading" {
            vec![c.text.clone()]
        } else {
            split_by_token_budget(&c.text, body_budget)
        };
        let split = pieces.len() > 1;
        for piece in pieces {
            let body = match &c.embedding_text {
                Some(e) if !split && !e.trim().is_empty() && tok.count(e, false) <= body_budget => {
                    e.clone()
                }
                _ => piece.clone(),
            };
            let mut embedding = format!("{header}\n\n{body}");
            if tok.count(&embedding, true) > budget {
                // Only an atomic heading or table gets here.
                embedding = tok
                    .split(&embedding, budget - specials)
                    .into_iter()
                    .next()
                    .unwrap_or_default();
            }
            let mut search = vec![title.as_str()];
            search.extend(c.heading_path.iter().map(String::as_str));
            search.push(&piece);
            let ordinal = out.len() as i64;
            out.push(ChunkRow {
                language: detect_language(&piece).map(str::to_owned),
                id: record::chunk_id(&document_id, ordinal as u64),
                document_id: document_id.clone(),
                file_id: file_id.to_owned(),
                kind: kind.to_owned(),
                ordinal,
                heading_path: c.heading_path.clone(),
                heading_level: if kind == "heading" {
                    c.heading_level.or(Some(depth as i64 + 1))
                } else {
                    None
                },
                search_text: search
                    .iter()
                    .filter(|s| !s.is_empty())
                    .copied()
                    .collect::<Vec<_>>()
                    .join("\n"),
                embedding_text: Some(embedding),
                token_count: Some(tok.count(&piece, false)),
                page_start: c.page_start,
                page_end: c.page_end,
                table_rows: if kind == "table" {
                    c.table_rows.clone()
                } else {
                    None
                },
                caption: None,
                parent_ordinal: parent,
                text: piece,
            });
        }
        if kind == "heading" {
            heading_at.truncate(depth);
            heading_at.resize(depth, None);
            heading_at.push(Some(out.len() as i64 - 1));
        }
    }
    let sample: String = out
        .iter()
        .map(|c| c.search_text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .take(20_000)
        .collect();
    let doc = DocumentRow {
        id: document_id,
        file_id: file_id.to_owned(),
        format: format.as_str().to_owned(),
        title: Some(title),
        author: result.author.clone().filter(|a| !a.trim().is_empty()),
        page_count: page_count.or(result.page_count),
        is_scanned: false,
        content_hash,
        attachment,
        language: result
            .language
            .clone()
            .filter(|l| l.len() == 2)
            .or_else(|| detect_language(&sample).map(str::to_owned)),
        script: detect_script(&sample).map(str::to_owned),
        pages: None,
    };
    (doc, out)
}

fn file_stem(name: &str) -> String {
    let stem = Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name);
    if stem.is_empty() {
        name.to_owned()
    } else {
        stem.to_owned()
    }
}

impl AiDocumentProcessor {
    /// Analyses `bytes` (cache first) or fails with a classed error.
    fn analyse(&self, input: &AnalysisInput, hash: &str) -> Result<AnalysisResult, AnalysisError> {
        let key = format!("ai|{}", converter_identity(self.analyzer.as_ref()));
        if let Some(v) = self.cache.as_ref().and_then(|c| c.get(hash, &key)) {
            if let Ok(r) = docmonk_ai::analysis::validate_value(v, input.page_count) {
                return Ok(r);
            }
        }
        if let Some(p) = input
            .parts
            .iter()
            .find(|p| !self.analyzer.accepts(p.kind()))
        {
            return Err(AnalysisError::new(
                AnalysisErrorKind::UnsupportedFormat,
                format!(
                    "the {} provider cannot take {} input",
                    self.analyzer.provider_id(),
                    p.kind().as_str()
                ),
            ));
        }
        let r = self.slots.run(|| self.analyzer.analyze(input))?;
        if let Some(c) = &self.cache {
            if let Ok(v) = serde_json::to_value(&r) {
                c.put(hash, &key, &v);
            }
        }
        Ok(r)
    }

    fn page_count(&self, path: &Path, format: DocumentFormat) -> Result<Option<i64>, ProcessError> {
        if format != DocumentFormat::Pdf {
            return Ok(None);
        }
        let n = crate::pdf::page_count(path).ok();
        if let (Some(n), Some(max)) = (n, self.max_pages) {
            if n > max {
                return Err(ProcessError {
                    code: SKIPPED_LIMIT.into(),
                    message: format!(
                        "{}: {n} pages exceeds documents.max_pages ({max})",
                        path.display()
                    ),
                    transient: false,
                });
            }
        }
        Ok(n.map(|n| n as i64))
    }

    /// Pure analysis of one file into its knowledge rows.
    pub fn knowledge(
        &self,
        path: &Path,
        file_id: &str,
        content_hash: Option<&str>,
    ) -> Result<FileKnowledge, ProcessError> {
        let Some(format) = detect_format(path) else {
            return Ok(FileKnowledge::default());
        };
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if size > self.max_file_bytes {
            return Err(too_large(path, size, self.max_file_bytes));
        }
        let page_count = self.page_count(path, format)?;
        let bytes = std::fs::read(path).map_err(|e| ProcessError {
            code: "conversion_error".into(),
            message: format!("{}: {e}", path.display()),
            transient: true,
        })?;
        let hash = format!("{:x}", Sha256::digest(&bytes));
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("document")
            .to_owned();
        let raw = (format == DocumentFormat::Eml).then(|| bytes.clone());
        let input = input_for(&file_name, format, bytes, self.max_file_bytes, page_count)
            .map_err(|e| error(path, e))?;
        let result = self.analyse(&input, &hash).map_err(|e| error(path, e))?;
        let document_id = record::document_id(file_id, None);
        let (doc, chunks) = rows(
            &result,
            &self.chunking,
            DocumentTarget {
                file_id,
                document_id: document_id.clone(),
                format,
                fallback_title: &file_stem(&file_name),
                page_count,
                content_hash: content_hash.map(str::to_owned),
                attachment: None,
            },
        );
        let mut knowledge = FileKnowledge {
            documents: vec![doc],
            chunks,
            ..FileKnowledge::default()
        };
        if let (Some(raw), Some(settings)) = (raw, self.attachments.as_ref().filter(|s| s.enabled))
        {
            self.add_attachments(path, &raw, file_id, &document_id, settings, &mut knowledge);
        }
        Ok(knowledge)
    }

    /// Email attachments as child documents: the MIME split is transport,
    /// each attachment is analysed on its own, and a failed attachment never
    /// fails its email.
    fn add_attachments(
        &self,
        path: &Path,
        raw: &[u8],
        file_id: &str,
        parent_id: &str,
        settings: &AttachmentSettings,
        knowledge: &mut FileKnowledge,
    ) {
        let extraction = match email::extract_attachments(raw, settings) {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(component = "documents", event = "email_attachments_unreadable", path = %path.display(), error = %e);
                knowledge.attachments.failed += 1;
                return;
            }
        };
        knowledge.attachments.seen = extraction.seen();
        knowledge.attachments.skipped += extraction.skipped.len();
        for part in &extraction.attachments {
            let name = part.display_name();
            let Some(ext) = email::attachment_extension(part) else {
                knowledge.attachments.skipped += 1;
                continue;
            };
            let Some(format) = detect_format(Path::new(&format!("attachment{ext}")))
                .filter(|f| *f != DocumentFormat::Eml)
            else {
                knowledge.attachments.skipped += 1;
                continue;
            };
            let hash = format!("{:x}", Sha256::digest(&part.payload));
            let outcome = input_for(
                &format!("attachment{ext}"),
                format,
                part.payload.clone(),
                self.max_file_bytes,
                None,
            )
            .and_then(|input| self.analyse(&input, &hash));
            match outcome {
                Ok(result) => {
                    let provenance = AttachmentProvenance {
                        parent_document_id: parent_id.to_owned(),
                        name: Some(name.clone()),
                        content_type: Some(part.content_type.clone()),
                        index: part.ordinal as i64,
                        content_id: part.content_id.clone(),
                    };
                    let id = record::document_id(file_id, Some(part.ordinal as u64));
                    let (doc, chunks) = rows(
                        &result,
                        &self.chunking,
                        DocumentTarget {
                            file_id,
                            document_id: id,
                            format,
                            fallback_title: &name,
                            page_count: None,
                            content_hash: Some(hash),
                            attachment: Some(provenance),
                        },
                    );
                    knowledge.documents.push(doc);
                    knowledge.chunks.extend(chunks);
                    knowledge.attachments.indexed += 1;
                    knowledge.attachments.bytes_processed += part.payload.len();
                }
                Err(e) => {
                    knowledge.attachments.failed += 1;
                    tracing::warn!(component = "documents", event = "email_attachment_failed",
                        path = %path.display(), attachment_index = part.ordinal, code = e.kind.code(),
                        error = %e.message.chars().take(500).collect::<String>());
                }
            }
        }
    }
}

impl Processor for AiDocumentProcessor {
    fn prepare(&self, input: &PrepareInput) -> Result<FileKnowledge, ProcessError> {
        self.knowledge(&input.path, &input.file_id, input.content_hash.as_deref())
    }
}
