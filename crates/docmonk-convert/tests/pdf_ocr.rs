//! PDF text layer, scanned detection, OCR policy, image OCR, page limit and
//! the conversion cache. OCR assertions need the pinned `ocrs` models in
//! `RAGMONK_OCR_MODELS_DIR`; CI sets `RAGMONK_REQUIRE_OCR=1` so they can
//! never silently skip there.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use docmonk_convert::cache::ConversionCache;
use docmonk_convert::converter::{
    ConversionError, DoclingConverter, DocumentConverter, PdfOptions,
};
use docmonk_convert::format::{detect_format, DocumentFormat};
use docmonk_convert::normalize::normalize;
use docmonk_convert::ocr::LazyOcr;
use docmonk_convert::process::{DocumentProcessor, SKIPPED_LIMIT};
use docmonk_documents::model::NormalizedDocument;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/documents")
        .join(name)
}

fn models() -> Option<PathBuf> {
    match std::env::var_os("RAGMONK_OCR_MODELS_DIR") {
        Some(d) => Some(PathBuf::from(d)),
        None => {
            assert!(
                std::env::var("RAGMONK_REQUIRE_OCR").as_deref() != Ok("1"),
                "RAGMONK_OCR_MODELS_DIR is required when RAGMONK_REQUIRE_OCR=1"
            );
            eprintln!("skipping OCR assertions: RAGMONK_OCR_MODELS_DIR not set");
            None
        }
    }
}

fn converter(
    ocr: &str,
    image_ocr: bool,
    models: Option<&Path>,
    cache: Option<&Path>,
) -> DoclingConverter {
    DoclingConverter {
        pdf: PdfOptions {
            ocr: ocr.into(),
            image_ocr,
            mode: "accurate".into(),
        },
        ocr: models.map(|m| Arc::new(LazyOcr::new(m.to_path_buf()))),
        cache: cache.map(ConversionCache::new),
    }
}

fn convert(c: &DoclingConverter, name: &str) -> Result<NormalizedDocument, ConversionError> {
    let p = fixture(name);
    let f = detect_format(&p).unwrap();
    c.convert(&p, f)
        .map(|j| normalize(&j, f == DocumentFormat::Pdf))
}

fn all_text(d: &NormalizedDocument) -> String {
    d.units
        .iter()
        .map(|u| u.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn text_layer_pdf_is_converted_without_ocr() {
    let d = convert(&converter("off", false, None, None), "sample.pdf").unwrap();
    let text = all_text(&d);
    assert!(text.contains("Sample PDF Title"), "{text}");
    assert!(text.contains("line of body text"), "{text}");
    assert_eq!(d.page_count, Some(1));
    assert!(!d.is_scanned);
    assert!(d.units.iter().all(|u| u.page_start == Some(1)));
}

#[test]
fn image_only_pdf_reads_as_scanned_without_ocr() {
    for name in ["scanned.pdf", "scanned_image.pdf"] {
        let d = convert(&converter("off", false, None, None), name).unwrap();
        assert!(d.units.is_empty(), "{name}");
        assert_eq!(d.page_count, Some(1), "{name}");
        assert!(d.is_scanned, "{name}");
    }
}

#[test]
fn missing_models_keep_the_plain_result() {
    let bogus = std::env::temp_dir().join("ragmonk-no-ocr-models");
    let d = convert(
        &converter("always", false, Some(&bogus), None),
        "scanned_image.pdf",
    )
    .unwrap();
    assert!(d.is_scanned && d.units.is_empty());
    let c = converter("auto", true, Some(&bogus), None);
    assert!(matches!(
        convert(&c, "sample_ocr.png"),
        Err(ConversionError::Unsupported(_))
    ));
}

#[test]
fn ocr_recovers_scanned_text_under_auto_and_always() {
    let Some(m) = models() else { return };
    for mode in ["auto", "always"] {
        for name in ["scanned_image.pdf", "scanned_gray.pdf"] {
            let d = convert(&converter(mode, false, Some(&m), None), name).unwrap();
            let text = all_text(&d).to_lowercase();
            assert!(text.contains("sample image title"), "{mode}/{name}: {text}");
            assert!(text.contains("ocr body text"), "{mode}/{name}: {text}");
            assert_eq!(d.units[0].page_start, Some(1));
        }
    }
    // A blank scan has nothing to recognize: the plain result is kept.
    let d = convert(&converter("auto", false, Some(&m), None), "scanned.pdf").unwrap();
    assert!(d.is_scanned && d.units.is_empty());
    // `auto` never OCRs a PDF that already has a text layer.
    let d = convert(&converter("auto", false, Some(&m), None), "sample.pdf").unwrap();
    assert!(all_text(&d).contains("Sample PDF Title"));
}

#[test]
fn images_need_image_ocr() {
    let c = converter("auto", false, None, None);
    assert!(matches!(
        convert(&c, "sample_ocr.png"),
        Err(ConversionError::Unsupported(_))
    ));
    let Some(m) = models() else { return };
    let d = convert(&converter("auto", true, Some(&m), None), "sample_ocr.png").unwrap();
    assert!(all_text(&d).to_lowercase().contains("sample image title"));
}

#[test]
fn oversized_pdf_is_skipped_by_limit() {
    let p = DocumentProcessor {
        converter: Arc::new(converter("off", false, None, None)),
        chunking: Default::default(),
        max_pages: Some(3),
        attachments: None,
        image_ocr: false,
    };
    let err = p
        .knowledge(&fixture("five_pages.pdf"), "f", None)
        .unwrap_err();
    assert_eq!(err.code, SKIPPED_LIMIT);
    assert!(p.knowledge(&fixture("sample.pdf"), "f", None).is_ok());
}

#[test]
fn conversions_are_cached_by_content_and_settings() {
    let tmp = tempfile::tempdir().unwrap();
    let c = converter("off", false, None, Some(tmp.path()));
    let a = convert(&c, "sample.pdf").unwrap();
    let entries = || walk(tmp.path()).len();
    assert_eq!(entries(), 1);
    let b = convert(&c, "sample.pdf").unwrap();
    assert_eq!(a, b);
    assert_eq!(entries(), 1, "second conversion is a cache hit");
    // Corrupt entries are ignored and rebuilt.
    for f in walk(tmp.path()) {
        std::fs::write(f, b"{not json").unwrap();
    }
    assert_eq!(convert(&c, "sample.pdf").unwrap(), a);
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}
