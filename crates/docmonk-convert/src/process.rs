//! The document processor's prepare half, including attachments: detect -> convert -> normalize -> metadata ->
//! chunk, all on the worker, with no storage access. An
//! `.eml` additionally yields one child document per converted attachment,
//! with provenance; a bad attachment never fails its email.

use std::path::Path;
use std::sync::Arc;

use docmonk_config::model::ChunkingConfig;
use docmonk_core::ids::record;
use docmonk_documents::chunker::chunk_document;
use docmonk_documents::model::{DocumentMetadata, NormalizedDocument};
use docmonk_indexing::coordinator::{PrepareInput, ProcessError, Processor};
use docmonk_storage::knowledge::{AttachmentProvenance, ChunkRow, DocumentRow, FileKnowledge};
use sha2::{Digest, Sha256};

use crate::converter::{ConversionError, DocumentConverter};
use crate::email::{self, AttachmentPart, AttachmentSettings};
use crate::format::{detect_format, DocumentFormat};
use crate::normalize::normalize;

/// Error code a processor uses to record a file as `skipped_limit`.
pub use docmonk_indexing::coordinator::SKIPPED_LIMIT;

pub struct DocumentProcessor {
    pub converter: Arc<dyn DocumentConverter>,
    pub chunking: ChunkingConfig,
    /// `documents.max_pages`: larger PDFs are recorded `skipped_limit`.
    pub max_pages: Option<usize>,
    /// `documents.email_attachment*`; `None` disables attachments.
    pub attachments: Option<AttachmentSettings>,
    pub image_ocr: bool,
}

/// One converted document before ids are assigned.
struct Converted {
    format: DocumentFormat,
    normalized: NormalizedDocument,
}

enum Outcome {
    Converted(Converted),
    /// Recognized, no derived content.
    Nothing,
}

fn conversion_error(path: &Path, m: impl std::fmt::Display) -> ProcessError {
    ProcessError {
        code: "conversion_error".into(),
        message: format!("{}: {m}", path.display()),
        transient: false,
    }
}

impl DocumentProcessor {
    fn page_limit(&self, path: &Path, format: DocumentFormat) -> Result<(), ProcessError> {
        if let (DocumentFormat::Pdf, Some(max)) = (format, self.max_pages) {
            if let Ok(n) = crate::pdf::page_count(path) {
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
        }
        Ok(())
    }

    fn convert(&self, path: &Path, format: DocumentFormat) -> Result<Outcome, ProcessError> {
        self.page_limit(path, format)?;
        match self.converter.convert(path, format) {
            Ok(json) => Ok(Outcome::Converted(Converted {
                format,
                normalized: normalize(&json, format == DocumentFormat::Pdf),
            })),
            Err(ConversionError::Unsupported(_)) => Ok(Outcome::Nothing),
            Err(ConversionError::Failed(m)) => Err(conversion_error(path, m)),
        }
    }

    fn rows(
        &self,
        c: &Converted,
        file_id: &str,
        document_id: String,
        title: Option<String>,
        content_hash: Option<String>,
        attachment: Option<AttachmentProvenance>,
    ) -> (DocumentRow, Vec<ChunkRow>) {
        let chunk_title = title.clone().unwrap_or_default();
        let chunks = chunk_document(&c.normalized, &self.chunking, &chunk_title, None)
            .into_iter()
            .enumerate()
            .map(|(i, ch)| ChunkRow {
                id: record::chunk_id(&document_id, i as u64),
                document_id: document_id.clone(),
                file_id: file_id.to_owned(),
                kind: ch.kind.as_str().to_owned(),
                ordinal: i as i64,
                heading_path: ch.heading_path,
                heading_level: ch.heading_level,
                text: ch.text,
                search_text: ch.search_text,
                embedding_text: Some(ch.contextual_text),
                token_count: Some(ch.token_count),
                page_start: ch.page_start,
                page_end: ch.page_end,
                table_rows: ch.table_rows,
                caption: ch.caption,
                parent_ordinal: ch.parent_index.map(|p| p as i64),
            })
            .collect();
        let doc = DocumentRow {
            id: document_id,
            file_id: file_id.to_owned(),
            format: c.format.as_str().to_owned(),
            title,
            author: None,
            page_count: c.normalized.page_count,
            is_scanned: c.normalized.is_scanned,
            content_hash,
            attachment,
        };
        (doc, chunks)
    }

    /// One attachment as a child document, or `Ok(None)` when skipped.
    fn attachment(
        &self,
        part: &AttachmentPart,
        file_id: &str,
        parent_id: &str,
        email_path: &Path,
    ) -> Result<Option<(DocumentRow, Vec<ChunkRow>)>, String> {
        let name = part.display_name();
        let skip = |reason: &str| {
            tracing::info!(component = "documents", event = "email_attachment_skipped",
                path = %email_path.display(), attachment_index = part.ordinal,
                content_type = %part.content_type, size = part.payload.len(), reason);
            Ok(None)
        };
        let Some(ext) = email::attachment_extension(part) else {
            return skip(email::SKIP_UNSUPPORTED);
        };
        let format = detect_format(Path::new(&format!("attachment{ext}"))).expect("supported");
        if format == DocumentFormat::Eml {
            return skip(email::SKIP_NESTED_MESSAGE);
        }
        if format == DocumentFormat::Image && !self.image_ocr {
            return skip(email::SKIP_IMAGE_OCR_DISABLED);
        }
        // Materialized under an internal name; the sender's filename never
        // touches the filesystem. Removed on every path.
        let dir = tempfile::Builder::new()
            .prefix("docmonk-attachment-")
            .tempdir()
            .map_err(|e| e.to_string())?;
        let path = dir.path().join(format!("attachment{ext}"));
        std::fs::write(&path, &part.payload).map_err(|e| e.to_string())?;
        let converted = match self.convert(&path, format) {
            Ok(Outcome::Converted(c)) => c,
            Ok(Outcome::Nothing) => return skip(email::SKIP_UNSUPPORTED),
            Err(e) if e.code == SKIPPED_LIMIT => return skip(email::SKIP_PAGE_LIMIT),
            Err(e) => return Err(e.message),
        };
        let meta =
            DocumentMetadata::from_normalized(&converted.normalized, format.as_str(), "attachment");
        let title = match meta.title {
            Some(t) if !t.is_empty() && t != "attachment" => t,
            _ => name.clone(),
        };
        let provenance = AttachmentProvenance {
            parent_document_id: parent_id.to_owned(),
            name: Some(name),
            content_type: Some(part.content_type.clone()),
            index: part.ordinal as i64,
            content_id: part.content_id.clone(),
        };
        let hash = format!("{:x}", Sha256::digest(&part.payload));
        let id = record::document_id(file_id, Some(part.ordinal as u64));
        Ok(Some(self.rows(
            &converted,
            file_id,
            id,
            Some(title),
            Some(hash),
            Some(provenance),
        )))
    }

    /// Pure conversion of one file into its knowledge rows.
    pub fn knowledge(
        &self,
        path: &Path,
        file_id: &str,
        content_hash: Option<&str>,
    ) -> Result<FileKnowledge, ProcessError> {
        let Some(format) = detect_format(path) else {
            return Ok(FileKnowledge::default());
        };
        let converted = match self.convert(path, format)? {
            Outcome::Converted(c) => c,
            Outcome::Nothing => return Ok(FileKnowledge::default()),
        };
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let meta =
            DocumentMetadata::from_normalized(&converted.normalized, format.as_str(), file_name);
        let document_id = record::document_id(file_id, None);
        let (doc, chunks) = self.rows(
            &converted,
            file_id,
            document_id.clone(),
            meta.title,
            content_hash.map(str::to_owned),
            None,
        );
        let mut knowledge = FileKnowledge {
            documents: vec![doc],
            chunks,
            ..FileKnowledge::default()
        };
        if format == DocumentFormat::Eml {
            if let Some(settings) = self.attachments.as_ref().filter(|s| s.enabled) {
                self.add_attachments(path, file_id, &document_id, settings, &mut knowledge);
            }
        }
        Ok(knowledge)
    }

    fn add_attachments(
        &self,
        path: &Path,
        file_id: &str,
        parent_id: &str,
        settings: &AttachmentSettings,
        knowledge: &mut FileKnowledge,
    ) {
        let extraction = match std::fs::read(path)
            .map_err(|e| e.to_string())
            .and_then(|raw| email::extract_attachments(&raw, settings))
        {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(component = "documents", event = "email_attachments_unreadable", path = %path.display(), error = %e);
                knowledge.attachments.failed += 1;
                return;
            }
        };
        knowledge.attachments.seen = extraction.seen();
        knowledge.attachments.skipped += extraction.skipped.len();
        for s in &extraction.skipped {
            tracing::info!(component = "documents", event = "email_attachment_skipped",
                path = %path.display(), attachment_index = s.ordinal, content_type = %s.content_type,
                size = s.size, reason = s.reason);
        }
        for part in &extraction.attachments {
            match self.attachment(part, file_id, parent_id, path) {
                Ok(Some((doc, chunks))) => {
                    knowledge.documents.push(doc);
                    knowledge.chunks.extend(chunks);
                    knowledge.attachments.indexed += 1;
                    knowledge.attachments.bytes_processed += part.payload.len();
                }
                Ok(None) => knowledge.attachments.skipped += 1,
                Err(e) => {
                    knowledge.attachments.failed += 1;
                    tracing::warn!(component = "documents", event = "email_attachment_failed",
                        path = %path.display(), attachment_index = part.ordinal,
                        content_type = %part.content_type, size = part.payload.len(),
                        error = %e.chars().take(500).collect::<String>());
                }
            }
        }
    }
}

impl Processor for DocumentProcessor {
    fn prepare(&self, input: &PrepareInput) -> Result<FileKnowledge, ProcessError> {
        self.knowledge(&input.path, &input.file_id, input.content_hash.as_deref())
    }
}
