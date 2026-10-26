//! PDF text layer, scanned detection, OCR policy, image OCR, page limit and
//! the conversion cache. OCR assertions need the pinned `ocrs` models in
//! `DOCMONK_OCR_MODELS_DIR`; CI sets `DOCMONK_REQUIRE_OCR=1` so they can
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
    match std::env::var_os("DOCMONK_OCR_MODELS_DIR") {
        Some(d) => Some(PathBuf::from(d)),
        None => {
            assert!(
                std::env::var("DOCMONK_REQUIRE_OCR").as_deref() != Ok("1"),
                "DOCMONK_OCR_MODELS_DIR is required when DOCMONK_REQUIRE_OCR=1"
            );
            eprintln!("skipping OCR assertions: DOCMONK_OCR_MODELS_DIR not set");
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
    let bogus = std::env::temp_dir().join("docmonk-no-ocr-models");
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

fn processor(c: DoclingConverter) -> DocumentProcessor {
    DocumentProcessor {
        converter: Arc::new(c),
        chunking: Default::default(),
        max_pages: None,
        attachments: None,
        image_ocr: false,
    }
}

/// How the pages of a PDF were read is recorded on its document, and a
/// page left without text by a missing or failed OCR is a visible notice
/// (ADR 0039), never a silently incomplete extraction.
#[test]
fn page_extraction_is_recorded_and_text_gaps_are_visible() {
    let bogus = std::env::temp_dir().join("docmonk-no-ocr-models");
    let k = processor(converter("auto", false, Some(&bogus), None))
        .knowledge(&fixture("scanned_image.pdf"), "f", None)
        .unwrap();
    let pages = k.documents[0].pages.clone().unwrap();
    assert_eq!(
        (pages.ocr.as_str(), pages.ocr_pages, pages.empty_pages),
        ("unavailable", 0, 1)
    );
    let notice = k.notice.unwrap();
    assert_eq!(notice.code, "ocr_unavailable");
    assert!(
        notice.message.contains("1 of 1 page(s) have no text"),
        "{notice:?}"
    );

    let k = processor(converter("off", false, None, None))
        .knowledge(&fixture("scanned_image.pdf"), "f", None)
        .unwrap();
    // Turned off by configuration: recorded, not a degraded file.
    assert_eq!(k.documents[0].pages.as_ref().unwrap().ocr, "disabled");
    assert_eq!(k.documents[0].pages.as_ref().unwrap().empty_pages, 1);
    assert!(k.notice.is_none());

    // A complete text layer needs no OCR and raises nothing.
    let k = processor(converter("auto", false, Some(&bogus), None))
        .knowledge(&fixture("sample.pdf"), "f", None)
        .unwrap();
    let pages = k.documents[0].pages.clone().unwrap();
    assert_eq!((pages.ocr.as_str(), pages.empty_pages), ("not_needed", 0));
    assert!(k.notice.is_none());
    assert_eq!(k.documents[0].language.as_deref(), Some("en"));
    assert_eq!(k.documents[0].script.as_deref(), Some("Latn"));

    let Some(m) = models() else { return };
    let k = processor(converter("auto", false, Some(&m), None))
        .knowledge(&fixture("scanned_image.pdf"), "f", None)
        .unwrap();
    let pages = k.documents[0].pages.clone().unwrap();
    assert_eq!(
        (pages.ocr.as_str(), pages.ocr_pages, pages.empty_pages),
        ("applied", 1, 0)
    );
    assert!(k.notice.is_none());
}
