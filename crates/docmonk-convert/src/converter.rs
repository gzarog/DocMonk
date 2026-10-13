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

    /// The PDF policy: plain text layer; OCR when
    /// `always`, or when `auto` and the plain result looks scanned; OCR
    /// failure or unavailability keeps the plain result.
    fn convert_pdf(&self, path: &Path, hash: &str) -> Result<Value, ConversionError> {
        let ocr_result = |this: &Self| -> Result<Option<Value>, ConversionError> {
            let Some(engine) = this.ocr_engine() else {
                return Ok(None);
            };
            this.cached(hash, "ocr:on", || match crate::pdf::ocr_pdf(path, engine) {
                Ok(v) => Ok(v),
                Err(e) => {
                    tracing::warn!(component = "ocr", event = "ocr_conversion_failed", error = %e);
                    Ok(None)
                }
            })
        };
        if self.pdf.ocr == "always" {
            if let Some(v) = ocr_result(self)? {
                return Ok(v);
            }
        }
        let plain = self
            .cached(hash, "ocr:off", || {
                crate::pdf::text_layer(path)
                    .map(Some)
                    .map_err(ConversionError::Failed)
            })?
            .unwrap_or(Value::Null);
        if self.pdf.ocr == "auto" && crate::normalize::normalize(&plain, true).is_scanned {
            if let Some(v) = ocr_result(self)? {
                return Ok(v);
            }
        }
        Ok(plain)
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
        DOCLING_PARSER_VERSION.into()
    }
}
