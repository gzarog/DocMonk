//! Rust-native document ingestion: format routing, the
//! `DocumentConverter` seam backed by docling.rs, the Docling-JSON
//! normalizer and the document processor.

pub mod ai;
pub mod cache;
pub mod converter;
pub mod email;
pub mod format;
pub mod normalize;
pub mod ocr;
pub mod pdf;
pub mod process;
pub mod render;

use std::sync::Arc;

use docmonk_config::DocMonkConfig;
use docmonk_indexing::coordinator::Registry;

/// The full registry: Rust-native code extraction plus Rust-native
/// document conversion and chunking. Version identities make any change in
/// the converter, chunker or tokenizer force a full rebuild.
pub fn registry(config: &DocMonkConfig) -> Registry {
    registry_with(config, &RegistryOptions::default())
}

/// Where OCR models and the conversion cache live.
#[derive(Debug, Clone, Default)]
pub struct RegistryOptions {
    /// Default `<home>/models/ocrs` (overridden by `DOCMONK_OCR_MODELS_DIR`).
    pub ocr_models_dir: Option<std::path::PathBuf>,
    /// Default: no cache.
    pub cache_dir: Option<std::path::PathBuf>,
    /// Embedding models root. Default `<home>/models` (overridden by
    /// `DOCMONK_MODELS_DIR`).
    pub models_root: Option<std::path::PathBuf>,
    /// The graph fact cache, used when `indexing.graph_fact_cache` is on.
    /// Default: none.
    pub graph_facts: Option<std::path::PathBuf>,
}

impl RegistryOptions {
    /// `<home>/models/ocrs` and `<home>/cache/conversion`.
    pub fn for_home(home: &docmonk_core::paths::Home) -> Self {
        Self {
            ocr_models_dir: Some(home.models_dir().join("ocrs")),
            models_root: Some(home.models_dir()),
            cache_dir: Some(home.cache_dir().join("conversion")),
            graph_facts: Some(graph_facts_path(home)),
        }
    }
}

/// `<home>/cache/graph-facts.db`.
pub fn graph_facts_path(home: &docmonk_core::paths::Home) -> std::path::PathBuf {
    home.cache_dir().join("graph-facts.db")
}

/// The graph fact cache when `indexing.graph_fact_cache` is on (`None`
/// when off, without a path, or when it cannot be opened: the graph stage
/// then parses as it always did).
pub fn open_graph_facts(
    config: &DocMonkConfig,
    path: Option<&std::path::Path>,
) -> Option<Arc<docmonk_storage::facts::FactCache>> {
    if !config.indexing.graph_fact_cache {
        return None;
    }
    match docmonk_storage::facts::FactCache::open(path?) {
        Ok(c) => Some(Arc::new(c)),
        Err(e) => {
            tracing::warn!(component = "relationships", event = "graph_facts_unavailable", error = %e);
            None
        }
    }
}

/// Checks that AI file analysis, when `documents.analysis.mode` is `ai`,
/// can run: the privacy gate, the provider and its credentials (ADR 0045).
/// Index runs call it before building the registry, so a blocked run stops
/// with the policy (exit 8) or config (exit 3) error before any file.
pub fn check_analysis(config: &DocMonkConfig) -> Result<(), docmonk_core::errors::DocMonkError> {
    if config.documents.analysis.is_ai() {
        docmonk_ai::create_analyzer(&config.documents.analysis, &config.privacy)?;
    }
    Ok(())
}

/// Fails every document file with the reason analysis cannot run (used when
/// a caller skipped [`check_analysis`]); nothing native is indexed.
struct UnavailableAnalysis(String);

impl docmonk_indexing::coordinator::Processor for UnavailableAnalysis {
    fn prepare(
        &self,
        input: &docmonk_indexing::coordinator::PrepareInput,
    ) -> Result<
        docmonk_storage::knowledge::FileKnowledge,
        docmonk_indexing::coordinator::ProcessError,
    > {
        if format::detect_format(&input.path).is_none() {
            return Ok(Default::default());
        }
        Err(docmonk_indexing::coordinator::ProcessError {
            code: "ai_analysis_not_configured".into(),
            message: format!("{}: {}", input.path.display(), self.0),
            transient: true,
        })
    }
}

pub fn registry_with(config: &DocMonkConfig, opts: &RegistryOptions) -> Registry {
    let d = &config.documents;
    let pdf = converter::PdfOptions {
        ocr: d.ocr.clone(),
        image_ocr: d.image_ocr,
        mode: d.pdf_mode.clone(),
    };
    let converter: Arc<dyn converter::DocumentConverter> = Arc::new(converter::DoclingConverter {
        pdf,
        ocr: Some(Arc::new(ocr::LazyOcr::new(ocr::models_dir(
            opts.ocr_models_dir.as_deref(),
        )))),
        cache: opts.cache_dir.clone().map(cache::ConversionCache::new),
    });
    // Relationships are not part of the primary index (see
    // `docmonk_code::graph_stage`), so `indexing.relationships_enabled`
    // never changes this registry or its identity.
    let mut r =
        docmonk_code::registry_with_facts(open_graph_facts(config, opts.graph_facts.as_deref()));
    // Toggling attachment extraction changes what an .eml yields, so it is
    // part of the identity (`+eml-attachments.1`).
    r.versions.converter_version = if d.email_attachments {
        format!(
            "{}+{}",
            converter.parser_version(),
            docmonk_documents::version::EMAIL_ATTACHMENTS_VERSION
        )
    } else {
        converter.parser_version()
    };
    // The normalized lexical projection of chunk FTS rows and the detected
    // languages (ADR 0039) are part of what chunks derive from.
    r.versions.chunker_version = format!(
        "{}+{}+{}",
        docmonk_documents::chunker::chunker_version_stamp(),
        docmonk_documents::version::CHUNK_PARENTS_VERSION,
        docmonk_core::lexnorm::LEXICAL_PROJECTION_VERSION
    );
    r.versions.embedding_text_version =
        Some(docmonk_documents::chunker::EMBEDDING_TEXT_VERSION.into());
    // The configured embedding model is part of the identity (ADR 0040):
    // changing `search.embedding_model` is a full rebuild on both
    // backends, so no unchanged file keeps vectors of the old model (a
    // server copies unchanged records forward as they are). Toggling
    // `search.semantic` does not change it.
    let model = docmonk_ml::manifest::configured_embedding_model(&config.search.embedding_model);
    r.versions.embedding_model_id = Some(format!("{}@{}", model.slug, model.fingerprint()));
    // Semantic vectors: every entity/chunk without a vector under the
    // current model is embedded before publication. A missing model leaves them
    // pending (logged) instead of failing the build.
    if config.search.semantic {
        r.finalizers
            .push(Arc::new(docmonk_ml::EmbeddingFinalizer::new(
                docmonk_ml::LazyEmbedder::new(
                    docmonk_ml::embedder::models_root(opts.models_root.as_deref()),
                    model,
                    config.indexing.embedding_batch_size,
                ),
            )));
    }
    let attachments = Some(email::AttachmentSettings {
        enabled: d.email_attachments,
        max_bytes: d.email_attachment_max_bytes.max(0) as usize,
        max_count: d.email_attachment_max_count.max(0) as usize,
        total_max_bytes: d.email_attachment_total_max_bytes.max(0) as usize,
    });
    if d.analysis.is_ai() {
        // AI file analysis (ADR 0045): the embedded extraction is bypassed.
        let eml = if d.email_attachments {
            format!("+{}", docmonk_documents::version::EMAIL_ATTACHMENTS_VERSION)
        } else {
            String::new()
        };
        match docmonk_ai::create_analyzer(&d.analysis, &config.privacy) {
            Ok(analyzer) => {
                r.versions.converter_version =
                    format!("{}{eml}", ai::converter_identity(analyzer.as_ref()));
                r.versions.chunker_version = format!(
                    "{}+ai+{}",
                    r.versions.chunker_version,
                    ai::AI_ANALYSIS_VERSION
                );
                r.document = Arc::new(ai::AiDocumentProcessor {
                    analyzer,
                    chunking: config.documents.chunking.clone(),
                    max_file_bytes: d.analysis.max_file_bytes.max(1) as u64,
                    max_pages: (d.max_pages > 0).then_some(d.max_pages as usize),
                    attachments,
                    cache: opts
                        .cache_dir
                        .clone()
                        .map(|c| cache::ConversionCache::new(c.join("ai"))),
                    slots: ai::Slots::new(d.analysis.concurrency.max(1) as usize),
                });
            }
            Err(e) => {
                r.versions.converter_version =
                    format!("{}+unavailable{eml}", ai::AI_ANALYSIS_VERSION);
                r.document = Arc::new(UnavailableAnalysis(e.message().to_owned()));
            }
        }
        return r.with_redaction(config.indexing.redact_secrets);
    }
    r.document = Arc::new(process::DocumentProcessor {
        converter,
        chunking: config.documents.chunking.clone(),
        max_pages: (d.max_pages > 0).then_some(d.max_pages as usize),
        attachments,
        image_ocr: d.image_ocr,
    });
    // Credentials in indexed text are redacted before storage (ADR 0042).
    r.with_redaction(config.indexing.redact_secrets)
}
