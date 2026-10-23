//! The `DocumentConverter` seam and its docling.rs implementation.

use std::path::Path;

use serde_json::Value;

use crate::format::DocumentFormat;

/// Converter identity, part of `parser_version` (a change rebuilds every
/// source's documents).
pub const DOCLING_PARSER_VERSION: &str = "docling.rs-1.89.0";

/// PDF/OCR behaviour (`documents.ocr`, `documents.image_ocr`, `pdf_mode`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfOptions {
    /// `off` | `auto` | `always`.
    pub ocr: String,
    pub image_ocr: bool,
    /// `accurate` | `fast`; both use the text layer in this build (the ML
    /// pipeline is an opt-in feature), so they share one result.
    pub mode: String,
}

impl Default for PdfOptions {
    fn default() -> Self {
        Self {
            ocr: "auto".into(),
            image_ocr: false,
            mode: "accurate".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConversionError {
    /// Recognized but not converted by this build (e.g. PDF/EML/images
    /// before their slices land); indexed with no derived content.
    #[error("{0} conversion is not available")]
    Unsupported(&'static str),
    #[error("conversion failed: {0}")]
    Failed(String),
}

/// Converts a file into Docling document JSON (the schema the normalizer
/// consumes). Implementations must not touch storage or the network.
pub trait DocumentConverter: Send + Sync {
    fn convert(&self, path: &Path, format: DocumentFormat) -> Result<Value, ConversionError>;
    fn parser_version(&self) -> String;
}

/// Rule-based docling.rs backends (no ML, no network), the PDF text layer
/// and `ocrs` OCR.
#[derive(Default)]
pub struct DoclingConverter {
    pub pdf: PdfOptions,
    pub ocr: Option<std::sync::Arc<crate::ocr::LazyOcr>>,
    pub cache: Option<crate::cache::ConversionCache>,
}

fn content_hash(path: &Path) -> Result<String, ConversionError> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).map_err(|e| ConversionError::Failed(e.to_string()))?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

impl DoclingConverter {
    fn ocr_engine(&self) -> Option<&crate::ocr::Ocr> {
        self.ocr.as_ref().and_then(|o| o.get().ok())
    }

    fn cached(
        &self,
        hash: &str,
        key: &str,
        make: impl FnOnce() -> Result<Option<Value>, ConversionError>,
    ) -> Result<Option<Value>, ConversionError> {
        let full_key = format!(
            "{key}|{DOCLING_PARSER_VERSION}|{}",
            crate::ocr::OCR_ENGINE_VERSION
        );
        if let Some(c) = &self.cache {
            if let Some(v) = c.get(hash, &full_key) {
                return Ok(Some(v));
            }
        }
        let v = make()?;
        if let (Some(c), Some(v)) = (&self.cache, &v) {
            c.put(hash, &full_key, v);
        }
        Ok(v)
    }

    /// The PDF policy (ADR 0039): the text layer, then OCR for every page
    /// whose text layer is (nearly) empty (`auto`), or for all pages
    /// (`always`), merged page by page ([`crate::pdf::merge_ocr`]): OCR
    /// adds the missing text and never replaces the text layer. How the
    /// pages were read is recorded under [`EXTRACTION_KEY`].
    fn convert_pdf(&self, path: &Path, hash: &str) -> Result<Value, ConversionError> {
        let plain = self
            .cached(hash, "ocr:off", || {
                crate::pdf::text_layer(path)
                    .map(Some)
                    .map_err(ConversionError::Failed)
            })?
            .unwrap_or(Value::Null);
        let (chars, pages) = crate::pdf::page_text_chars(&plain);
        let sparse_pages: std::collections::BTreeSet<usize> = chars
            .iter()
            .filter(|(_, n)| (**n as f64) < crate::normalize::SCANNED_CHARS_PER_PAGE_THRESHOLD)
            .map(|(p, _)| *p)
            .collect();
        let sparse = sparse_pages.len();
        let wanted = match self.pdf.ocr.as_str() {
            "always" => pages > 0,
            "auto" => sparse > 0,
            _ => false,
        };
        let mut report = PdfExtraction {
            pages,
            text_layer_pages: pages - sparse,
            ..PdfExtraction::default()
        };
        let mut doc = plain;
        report.ocr = if !wanted {
            if sparse > 0 && self.pdf.ocr == "off" {
                "disabled"
            } else {
                "not_needed"
            }
        } else if let Some(engine) = self.ocr_engine() {
            // `auto` reads only the pages without a text layer.
            let (key, only) = if self.pdf.ocr == "always" || sparse == pages {
                ("ocr:on".to_owned(), None)
            } else {
                use sha2::{Digest, Sha256};
                let list: Vec<String> = sparse_pages.iter().map(usize::to_string).collect();
                let digest = format!("{:x}", Sha256::digest(list.join(",").as_bytes()));
                (format!("ocr:pages:{}", &digest[..16]), Some(&sparse_pages))
            };
            let ocr = self.cached(hash, &key, || match crate::pdf::ocr_pdf(path, engine, only) {
                Ok(v) => Ok(Some(v.unwrap_or(Value::Null))),
                Err(e) => {
                    tracing::warn!(component = "ocr", event = "ocr_conversion_failed", error = %e);
                    Ok(None)
                }
            })?;
            match ocr {
                None => "failed",
                Some(v) if v.is_null() => "no_images",
                Some(v) => {
                    let (merged, stats) = crate::pdf::merge_ocr(doc, &v);
                    doc = merged;
                    report.ocr_pages = stats.ocr_pages;
                    report.ocr_duplicate_blocks = stats.duplicate_blocks;
                    "applied"
                }
            }
        } else {
            "unavailable"
        };
        let (after, _) = crate::pdf::page_text_chars(&doc);
        report.empty_pages = after.values().filter(|n| **n == 0).count();
        if doc.is_object() {
            doc[EXTRACTION_KEY] = report.to_json();
        }
        Ok(doc)
    }
}

/// Docling-JSON key under which the PDF policy records how a PDF's pages
/// were read ([`PdfExtraction`]); the normalizer ignores it.
pub const EXTRACTION_KEY: &str = "docmonk_extraction";

/// Identity of the PDF page policy, part of `parser_version`. (1: per-page
/// OCR merged with the text layer, extraction report.)
pub const PDF_PAGES_VERSION: &str = "pdf-pages-1";

/// How one PDF's pages were read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PdfExtraction {
    pub pages: usize,
    /// Pages whose text layer has text.
    pub text_layer_pages: usize,
    /// `not_needed`, `applied`, `no_images` (nothing to recognize),
    /// `unavailable` (no verified OCR models), `failed`, or `disabled`
    /// (`documents.ocr off` while pages lack text).
    pub ocr: &'static str,
    /// Pages that gained OCR text.
    pub ocr_pages: usize,
    /// OCR blocks dropped as repeats of the text layer.
    pub ocr_duplicate_blocks: usize,
    /// Pages still without any text.
    pub empty_pages: usize,
}

impl PdfExtraction {
    fn to_json(&self) -> Value {
        serde_json::json!({
            "pages": self.pages,
            "text_layer_pages": self.text_layer_pages,
            "ocr": self.ocr,
            "ocr_pages": self.ocr_pages,
            "ocr_duplicate_blocks": self.ocr_duplicate_blocks,
            "empty_pages": self.empty_pages,
        })
    }
}

impl DocumentConverter for DoclingConverter {
    fn convert(&self, path: &Path, format: DocumentFormat) -> Result<Value, ConversionError> {
        match format {
            DocumentFormat::Pdf => {
                let hash = content_hash(path)?;
                return self.convert_pdf(path, &hash);
            }
            DocumentFormat::Image => {
                if !self.pdf.image_ocr {
                    return Err(ConversionError::Unsupported("image"));
                }
                let Some(engine) = self.ocr_engine() else {
                    return Err(ConversionError::Unsupported(
                        "image (OCR models unavailable)",
                    ));
                };
                let hash = content_hash(path)?;
                return self
                    .cached(&hash, "image:ocr", || {
                        crate::pdf::ocr_image(path, engine)
                            .map(Some)
                            .map_err(ConversionError::Failed)
                    })
                    .map(|v| v.unwrap_or(Value::Null));
            }
            _ => {}
        }
        use docling::InputFormat as F;
        let input = match format {
            DocumentFormat::Docx => F::Docx,
            DocumentFormat::Pptx => F::Pptx,
            DocumentFormat::Xlsx => F::Xlsx,
            DocumentFormat::Html => F::Html,
            // Plain text is routed through the Markdown backend.
            DocumentFormat::Markdown | DocumentFormat::Txt => F::Md,
            DocumentFormat::Csv => F::Csv,
            DocumentFormat::Odt => F::Odt,
            DocumentFormat::Ods => F::Ods,
            DocumentFormat::Odp => F::Odp,
            DocumentFormat::Epub => F::Epub,
            DocumentFormat::Eml => F::Email,
            DocumentFormat::Pdf | DocumentFormat::Image => unreachable!("handled above"),
        };
        let bytes = std::fs::read(path).map_err(|e| ConversionError::Failed(e.to_string()))?;
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("document")
            .to_owned();
        let source = docling::SourceDocument::from_bytes(name, input, bytes);
        // Backends may panic on adversarial input; contain it per file.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            docling::DocumentConverter::new().convert(source)
        }))
        .map_err(|_| ConversionError::Failed("converter panicked".into()))?
        .map_err(|e| ConversionError::Failed(e.to_string()))?;
        Ok(result.document.export_to_json_value())
    }

    fn parser_version(&self) -> String {
        format!("{DOCLING_PARSER_VERSION}+{PDF_PAGES_VERSION}")
    }
}
