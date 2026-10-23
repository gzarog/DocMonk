//! PDF and image conversion: the PDF text layer (docling.rs `pdf-text`,
//! no ML), page-image extraction for OCR, Docling-JSON assembly for OCR
//! output, and the per-page merge of the two ([`merge_ocr`], ADR 0039).

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

/// OCR the embedded images of every page, or only of the pages in `only`.
/// `None` when those pages have no images.
pub fn ocr_pdf(
    path: &Path,
    ocr: &Ocr,
    only: Option<&std::collections::BTreeSet<usize>>,
) -> Result<Option<Value>, String> {
    let doc = Document::load(path).map_err(|e| format!("could not open PDF: {e}"))?;
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("document");
    let mut pages = Vec::new();
    let mut any_image = false;
    for (page_no, page_id) in doc.get_pages() {
        if only.is_some_and(|o| !o.contains(&(page_no as usize))) {
            continue;
        }
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

/// Characters of text per page of a Docling document (pages without
/// text are listed with 0), and its page count.
pub fn page_text_chars(doc: &Value) -> (std::collections::BTreeMap<usize, usize>, usize) {
    let mut chars: std::collections::BTreeMap<usize, usize> = Default::default();
    let pages = doc["pages"].as_object().map_or(0, serde_json::Map::len);
    for p in 1..=pages {
        chars.insert(p, 0);
    }
    for item in doc["texts"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(doc["tables"].as_array().into_iter().flatten())
    {
        let n = item["text"]
            .as_str()
            .map_or(0, |t| t.trim().chars().count())
            + item["data"]["table_cells"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| c["text"].as_str())
                .map(|t| t.chars().count())
                .sum::<usize>();
        for p in item["prov"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p["page_no"].as_u64())
        {
            *chars.entry(p as usize).or_default() += n;
        }
    }
    let count = pages.max(chars.keys().next_back().copied().unwrap_or(0));
    (chars, count)
}

/// What a merge added.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeStats {
    /// Pages that gained OCR text.
    pub ocr_pages: usize,
    /// OCR blocks added.
    pub added_blocks: usize,
    /// OCR blocks dropped because the page's text layer already had them.
    pub duplicate_blocks: usize,
}

/// Words of `text`, folded, for comparing OCR output with a text layer.
fn folded_words(text: &str) -> Vec<String> {
    docmonk_core::lexnorm::words(text)
        .map(docmonk_core::lexnorm::fold)
        .collect()
}

/// Whether the OCR `block` repeats text the page already has: its folded
/// words occur in the page's words for at least 80% (OCR misreads a few).
fn is_duplicate(block: &str, page_words: &std::collections::HashSet<String>) -> bool {
    let words = folded_words(block);
    if words.is_empty() {
        return true;
    }
    let known = words.iter().filter(|w| page_words.contains(*w)).count();
    known * 5 >= words.len() * 4
}

/// The first page an item (or the first of a group's children) is on.
fn first_page(doc: &Value, item: &Value, depth: usize) -> Option<usize> {
    if let Some(p) = item["prov"]
        .as_array()
        .and_then(|p| p.iter().filter_map(|p| p["page_no"].as_u64()).min())
    {
        return Some(p as usize);
    }
    if depth > 8 {
        return None;
    }
    item["children"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["$ref"].as_str())
        .filter_map(|r| resolve_ref(doc, r))
        .find_map(|c| first_page(doc, c, depth + 1))
}

fn resolve_ref<'a>(doc: &'a Value, r: &str) -> Option<&'a Value> {
    let mut parts = r.trim_start_matches("#/").split('/');
    let coll = parts.next()?;
    let idx: usize = parts.next()?.parse().ok()?;
    doc.get(coll)?.get(idx)
}

/// Adds `ocr`'s text to the text layer `native`, page by page: an OCR
/// block joins its page unless the page's text layer already holds it.
/// The text layer is never removed or rewritten, every added block keeps
/// its page (`prov`), and blocks are placed after the page's existing
/// content, so reading order stays by page.
pub fn merge_ocr(mut native: Value, ocr: &Value) -> (Value, MergeStats) {
    use std::collections::{BTreeMap, HashSet};
    let mut by_page: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for t in ocr["texts"].as_array().into_iter().flatten() {
        let Some(text) = t["text"].as_str().map(str::trim).filter(|t| !t.is_empty()) else {
            continue;
        };
        let Some(p) = t["prov"][0]["page_no"].as_u64() else {
            continue;
        };
        by_page.entry(p as usize).or_default().push(text.to_owned());
    }
    // Each page's existing words.
    let mut page_words: BTreeMap<usize, HashSet<String>> = BTreeMap::new();
    for item in native["texts"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(native["tables"].as_array().into_iter().flatten())
    {
        let mut text = item["text"].as_str().unwrap_or_default().to_owned();
        for c in item["data"]["table_cells"].as_array().into_iter().flatten() {
            text.push(' ');
            text.push_str(c["text"].as_str().unwrap_or_default());
        }
        for p in item["prov"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p["page_no"].as_u64())
        {
            page_words
                .entry(p as usize)
                .or_default()
                .extend(folded_words(&text));
        }
    }
    let mut stats = MergeStats::default();
    let mut added: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for (page, blocks) in by_page {
        let words = page_words.entry(page).or_default();
        for b in blocks {
            if is_duplicate(&b, words) {
                stats.duplicate_blocks += 1;
            } else {
                words.extend(folded_words(&b));
                added.entry(page).or_default().push(b);
            }
        }
    }
    if added.is_empty() {
        return (native, stats);
    }
    stats.ocr_pages = added.len();
    // Where each page's blocks go: before the first body child that starts
    // on a later page.
    let child_pages: Vec<Option<usize>> = native["body"]["children"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            c["$ref"]
                .as_str()
                .and_then(|r| resolve_ref(&native, r))
                .and_then(|item| first_page(&native, item, 0))
        })
        .collect();
    if !native["texts"].is_array() {
        native["texts"] = json!([]);
    }
    if !native["body"]["children"].is_array() {
        native["body"] = json!({"self_ref": "#/body", "children": []});
    }
    let mut inserts: Vec<(usize, Value)> = Vec::new();
    for (page, blocks) in added {
        let at = child_pages
            .iter()
            .position(|p| p.is_some_and(|p| p > page))
            .unwrap_or(child_pages.len());
        for b in blocks {
            let texts = native["texts"].as_array_mut().expect("texts");
            let idx = texts.len();
            texts.push(json!({
                "self_ref": format!("#/texts/{idx}"),
                "label": "paragraph",
                "content_layer": "body",
                "text": b,
                "prov": [{"page_no": page}],
                "children": [],
            }));
            stats.added_blocks += 1;
            inserts.push((at, json!({"$ref": format!("#/texts/{idx}")})));
        }
    }
    let children = native["body"]["children"].as_array_mut().expect("children");
    // Insert from the back so earlier positions stay valid; blocks of one
    // position keep their order.
    for (at, r) in inserts.into_iter().rev() {
        children.insert(at, r);
    }
    if let Some(pages) = native["pages"].as_object_mut() {
        for p in stats_pages(ocr) {
            pages
                .entry(p.to_string())
                .or_insert_with(|| json!({"page_no": p}));
        }
    }
    (native, stats)
}

fn stats_pages(doc: &Value) -> Vec<usize> {
    doc["pages"]
        .as_object()
        .map(|m| m.keys().filter_map(|k| k.parse().ok()).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ocr_fills_text_less_pages_and_never_repeats_or_drops_the_text_layer() {
        let native = assemble(
            "doc",
            &[
                (
                    1,
                    vec!["Refunds above 500 euros need finance approval.".into()],
                ),
                (2, vec![]),
                (3, vec!["Appendix: contacts".into()]),
            ],
        );
        let ocr = assemble(
            "doc",
            &[
                // A misread copy of the text layer: dropped.
                (
                    1,
                    vec!["Refunds above 5OO euros need finance approval.".into()],
                ),
                // The scanned page: added.
                (2, vec!["Signed scan: approved by the night shift.".into()]),
                (
                    3,
                    vec!["Appendix: contacts".into(), "Phone 555-0100".into()],
                ),
            ],
        );
        let (merged, stats) = merge_ocr(native, &ocr);
        assert_eq!(
            stats,
            MergeStats {
                ocr_pages: 2,
                added_blocks: 2,
                duplicate_blocks: 2
            }
        );
        let texts: Vec<(&str, u64)> = merged["body"]["children"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                let t = resolve_ref(&merged, c["$ref"].as_str().unwrap()).unwrap();
                (
                    t["text"].as_str().unwrap(),
                    t["prov"][0]["page_no"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            texts,
            [
                ("Refunds above 500 euros need finance approval.", 1),
                ("Signed scan: approved by the night shift.", 2),
                ("Appendix: contacts", 3),
                ("Phone 555-0100", 3),
            ]
        );
        let (chars, pages) = page_text_chars(&merged);
        assert_eq!(pages, 3);
        assert!(chars.values().all(|n| *n > 0), "{chars:?}");
    }

    #[test]
    fn a_text_layer_without_gaps_is_returned_unchanged() {
        let native = assemble("doc", &[(1, vec!["All text is here.".into()])]);
        let ocr = assemble("doc", &[(1, vec!["All text is here.".into()])]);
        let (merged, stats) = merge_ocr(native.clone(), &ocr);
        assert_eq!(merged, native);
        assert_eq!(stats.added_blocks, 0);
    }
}
