//! PDF and image conversion: the PDF text layer (docling.rs `pdf-text`,
//! no ML), page-image extraction for OCR, and Docling-JSON assembly for
//! OCR output.

use std::path::Path;

use lopdf::Document;
use serde_json::{json, Value};

use crate::ocr::Ocr;

/// Number of pages, without converting anything.
pub fn page_count(path: &Path) -> Result<usize, String> {
    let doc = Document::load(path).map_err(|e| format!("could not open PDF: {e}"))?;
    Ok(doc.get_pages().len())
}

/// The embedded text layer as Docling JSON. A PDF without any text layer
/// yields an empty document that still carries its pages (so it reads as
/// scanned).
pub fn text_layer(path: &Path) -> Result<Value, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("document")
        .to_owned();
    let source =
        docling::SourceDocument::from_bytes(name.clone(), docling::InputFormat::Pdf, bytes);
    let converted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        docling::DocumentConverter::new().convert(source)
    }))
    .map_err(|_| "PDF converter panicked".to_owned())?;
    match converted {
        Ok(r) => Ok(r.document.export_to_json_value()),
        Err(e) if e.to_string().contains("no embedded text layer") => {
            Ok(empty_document(&name, page_count(path)?))
        }
        Err(e) => Err(e.to_string()),
    }
}

fn empty_document(name: &str, pages: usize) -> Value {
    assemble(
        name,
        &(1..=pages).map(|p| (p, Vec::new())).collect::<Vec<_>>(),
    )
}

/// Docling JSON with one paragraph per text block, each with its page.
pub fn assemble(name: &str, pages: &[(usize, Vec<String>)]) -> Value {
    let mut texts = Vec::new();
    let mut children = Vec::new();
    let mut page_map = serde_json::Map::new();
    for (page_no, blocks) in pages {
        page_map.insert(page_no.to_string(), json!({"page_no": page_no}));
        for block in blocks {
            let idx = texts.len();
            children.push(json!({"$ref": format!("#/texts/{idx}")}));
            texts.push(json!({
                "self_ref": format!("#/texts/{idx}"),
                "label": "paragraph",
                "content_layer": "body",
                "text": block,
                "prov": [{"page_no": page_no}],
                "children": [],
            }));
        }
    }
    json!({
        "name": name,
        "body": {"self_ref": "#/body", "children": children},
        "texts": texts,
        "tables": [],
        "pictures": [],
        "groups": [],
        "pages": page_map,
    })
}

fn decode_image(img: &lopdf::xobject::PdfImage<'_>) -> Option<image::DynamicImage> {
    let filters = img.filters.clone().unwrap_or_default();
    if filters.iter().any(|f| f == "DCTDecode" || f == "JPXDecode") {
        return image::load_from_memory(img.content).ok();
    }
    // Raw (already Flate-decoded by lopdf where applicable) 8-bit samples.
    let (w, h) = (
        u32::try_from(img.width).ok()?,
        u32::try_from(img.height).ok()?,
    );
    if img.bits_per_component != Some(8) {
        return None;
    }
    let data = if filters.iter().any(|f| f == "FlateDecode") {
        flate_decode(img.content)?
    } else {
        img.content.to_vec()
    };
    match img.color_space.as_deref() {
        Some("DeviceRGB") => {
            image::RgbImage::from_raw(w, h, data).map(image::DynamicImage::ImageRgb8)
        }
        Some("DeviceGray") => {
            image::GrayImage::from_raw(w, h, data).map(image::DynamicImage::ImageLuma8)
        }
        _ => None,
    }
}

fn flate_decode(input: &[u8]) -> Option<Vec<u8>> {
    let mut dict = lopdf::Dictionary::new();
    dict.set("Filter", lopdf::Object::Name(b"FlateDecode".to_vec()));
    lopdf::Stream::new(dict, input.to_vec())
        .decompressed_content()
        .ok()
}

/// OCR every page's embedded images. `None` when the PDF has no images.
pub fn ocr_pdf(path: &Path, ocr: &Ocr) -> Result<Option<Value>, String> {
    let doc = Document::load(path).map_err(|e| format!("could not open PDF: {e}"))?;
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("document");
    let mut pages = Vec::new();
    let mut any_image = false;
    for (page_no, page_id) in doc.get_pages() {
        let mut blocks = Vec::new();
        for img in doc.get_page_images(page_id).unwrap_or_default() {
            let Some(decoded) = decode_image(&img) else {
                continue;
            };
            any_image = true;
            let text = ocr.image_text(decoded)?;
            let block = text.trim();
            if !block.is_empty() {
                blocks.push(block.to_owned());
            }
        }
        pages.push((page_no as usize, blocks));
    }
    Ok(any_image.then(|| assemble(name, &pages)))
}

/// OCR a raster image file into a one-page document.
pub fn ocr_image(path: &Path, ocr: &Ocr) -> Result<Value, String> {
    let img = image::open(path).map_err(|e| format!("could not decode image: {e}"))?;
    let text = ocr.image_text(img)?;
    let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
    let blocks: Vec<String> = text
        .trim()
        .is_empty()
        .then(Vec::new)
        .unwrap_or_else(|| vec![text.trim().to_owned()]);
    Ok(assemble(name, &[(1, blocks)]))
}
