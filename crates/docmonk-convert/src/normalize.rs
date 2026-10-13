//! Docling document JSON -> RagMonk's canonical normalized document.
//!
//! Operates on the Docling document JSON schema (`texts`, `tables`,
//! `pictures`, `groups`, `body`, `pages`) that both Docling
//! (`export_to_dict`) and docling.rs (`export_to_json_value`) produce, and
//! reproduces `DoclingDocument.iterate_items()` (body layer only, groups
//! flattened, picture children limited to captions) exactly.

use std::collections::{HashMap, HashSet};

use docmonk_documents::model::{NormalizedDocument, NormalizedUnit, UnitKind};
use serde_json::Value;

/// Below this many characters per PDF page, text counts as absent.
pub const SCANNED_CHARS_PER_PAGE_THRESHOLD: f64 = 50.0;

const SKIPPED_TEXT_LABELS: &[&str] = &["page_header", "page_footer"];

/// Shared by `is_scanned` and the PDF auto-OCR trigger.
pub fn is_low_text_density(
    page_count: Option<i64>,
    total_text_chars: usize,
    pages_with_text: usize,
) -> bool {
    match page_count {
        None | Some(0) => total_text_chars == 0,
        Some(n) => {
            total_text_chars == 0
                || (total_text_chars as f64 / n as f64) < SCANNED_CHARS_PER_PAGE_THRESHOLD
                || (pages_with_text as i64) * 2 < n
        }
    }
}

fn resolve<'a>(doc: &'a Value, cref: &str) -> Option<&'a Value> {
    let mut parts = cref.trim_start_matches("#/").split('/');
    let collection = parts.next()?;
    match parts.next() {
        None => doc.get(collection),
        Some(idx) => doc.get(collection)?.get(idx.parse::<usize>().ok()?),
    }
}

fn cref(v: &Value) -> Option<&str> {
    v.get("$ref")
        .or_else(|| v.get("cref"))
        .and_then(Value::as_str)
}

fn collection(r: &str) -> &str {
    r.trim_start_matches("#/").split('/').next().unwrap_or("")
}

/// `iterate_items()`: (self_ref, item) in document order.
fn iterate_items(doc: &Value) -> Vec<(String, &Value)> {
    fn walk<'a>(doc: &'a Value, r: &str, node: &'a Value, out: &mut Vec<(String, &'a Value)>) {
        let coll = collection(r);
        let is_group = coll == "groups" || coll == "body" || coll == "furniture";
        let layer = node
            .get("content_layer")
            .and_then(Value::as_str)
            .unwrap_or("body");
        if !is_group && layer == "body" {
            out.push((r.to_owned(), node));
        }
        let is_picture = coll == "pictures";
        let allowed: HashSet<&str> = if is_picture {
            node.get("captions")
                .and_then(Value::as_array)
                .map(|c| c.iter().filter_map(cref).collect())
                .unwrap_or_default()
        } else {
            HashSet::new()
        };
        for child in node
            .get("children")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(cr) = cref(child) else { continue };
            if is_picture && !allowed.contains(cr) {
                continue;
            }
            if let Some(c) = resolve(doc, cr) {
                walk(doc, cr, c, out);
            }
        }
    }
    let mut out = Vec::new();
    if let Some(body) = doc.get("body") {
        walk(doc, "#/body", body, &mut out);
    }
    out
}

fn page_range(item: &Value) -> (Option<i64>, Option<i64>) {
    let pages: Vec<i64> = item
        .get("prov")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| p.get("page_no").and_then(Value::as_i64))
        .collect();
    (pages.iter().min().copied(), pages.iter().max().copied())
}

fn table_rows(table: &Value) -> Vec<Vec<String>> {
    let data = &table["data"];
    let rows = data["num_rows"].as_u64().unwrap_or(0) as usize;
    let cols = data["num_cols"].as_u64().unwrap_or(0) as usize;
    let mut grid = vec![vec![String::new(); cols]; rows];
    for cell in data["table_cells"].as_array().into_iter().flatten() {
        let r = cell["start_row_offset_idx"].as_i64().unwrap_or(-1);
        let c = cell["start_col_offset_idx"].as_i64().unwrap_or(-1);
        if r >= 0 && c >= 0 && (r as usize) < rows && (c as usize) < cols {
            grid[r as usize][c as usize] = cell["text"].as_str().unwrap_or("").to_owned();
        }
    }
    grid
}

fn header_row_count(table: &Value) -> usize {
    let data = &table["data"];
    let header_rows: HashSet<i64> = data["table_cells"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["column_header"].as_bool().unwrap_or(false))
        .filter_map(|c| c["start_row_offset_idx"].as_i64())
        .collect();
    let mut count = 0;
    while header_rows.contains(&(count as i64)) {
        count += 1;
    }
    if count == 0 && data["num_rows"].as_u64().unwrap_or(0) > 0 {
        1
    } else {
        count
    }
}

/// Strips Unicode whitespace from both ends, like `str::trim`.
fn strip(s: &str) -> &str {
    docmonk_documents::tokenization::py_strip(s)
}

/// Normalizes Docling document JSON; `is_pdf` enables the scanned check.
pub fn normalize(doc: &Value, is_pdf: bool) -> NormalizedDocument {
    let page_count = doc
        .get("pages")
        .and_then(Value::as_object)
        .map(|p| p.len() as i64)
        .filter(|n| *n > 0);

    let mut caption_by_table: HashMap<String, String> = HashMap::new();
    let mut consumed: HashSet<String> = HashSet::new();
    for (i, table) in doc
        .get("tables")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let self_ref = table
            .get("self_ref")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("#/tables/{i}"));
        let mut text = String::new();
        for c in table
            .get("captions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(r) = cref(c) {
                if let Some(t) = resolve(doc, r)
                    .and_then(|x| x.get("text"))
                    .and_then(Value::as_str)
                {
                    text.push_str(t);
                }
                consumed.insert(r.to_owned());
            }
        }
        let text = strip(&text);
        if !text.is_empty() {
            caption_by_table.insert(self_ref, text.to_owned());
        }
    }

    let mut units: Vec<NormalizedUnit> = Vec::new();
    let mut stack: Vec<(i64, usize, String)> = Vec::new();
    let mut total_text_chars = 0usize;
    let mut pages_with_text: HashSet<i64> = HashSet::new();
    let path_of = |stack: &[(i64, usize, String)]| {
        stack.iter().map(|(_, _, t)| t.clone()).collect::<Vec<_>>()
    };

    for (self_ref, item) in iterate_items(doc) {
        let coll = collection(&self_ref);
        let label = item.get("label").and_then(Value::as_str).unwrap_or("");
        let is_text = coll == "texts";
        let is_table = coll == "tables";
        if !is_text && !is_table {
            continue;
        }
        if is_text && (SKIPPED_TEXT_LABELS.contains(&label) || consumed.contains(&self_ref)) {
            continue;
        }
        let (page_start, page_end) = page_range(item);
        let text = item.get("text").and_then(Value::as_str).unwrap_or("");

        if is_text && (label == "title" || label == "section_header") {
            let level = if label == "title" {
                0
            } else {
                item.get("level").and_then(Value::as_i64).unwrap_or(1)
            };
            while stack.last().is_some_and(|(l, _, _)| *l >= level) {
                stack.pop();
            }
            let index = units.len();
            units.push(NormalizedUnit {
                kind: UnitKind::Heading,
                text: text.to_owned(),
                heading_level: Some(level),
                heading_path: path_of(&stack),
                parent_index: stack.last().map(|(_, i, _)| *i),
                page_start,
                page_end,
                table_rows: None,
                caption: None,
                header_row_count: 0,
            });
            stack.push((level, index, text.to_owned()));
            total_text_chars += text.chars().count();
            if !strip(text).is_empty() {
                if let (Some(a), Some(b)) = (page_start, page_end) {
                    pages_with_text.extend(a..=b);
                }
            }
            continue;
        }

        let heading_path = path_of(&stack);
        let parent_index = stack.last().map(|(_, i, _)| *i);
        if is_table {
            units.push(NormalizedUnit {
                kind: UnitKind::Table,
                text: String::new(),
                heading_level: None,
                heading_path,
                parent_index,
                page_start,
                page_end,
                table_rows: Some(table_rows(item)),
                caption: caption_by_table.get(&self_ref).cloned(),
                header_row_count: header_row_count(item),
            });
            continue;
        }
        let t = strip(text);
        if t.is_empty() {
            continue;
        }
        total_text_chars += t.chars().count();
        if let (Some(a), Some(b)) = (page_start, page_end) {
            pages_with_text.extend(a..=b);
        }
        units.push(NormalizedUnit {
            kind: UnitKind::Paragraph,
            text: t.to_owned(),
            heading_level: None,
            heading_path,
            parent_index,
            page_start,
            page_end,
            table_rows: None,
            caption: None,
            header_row_count: 0,
        });
    }

    let title = units
        .iter()
        .find(|u| u.kind == UnitKind::Heading && u.heading_level == Some(0))
        .or_else(|| units.iter().find(|u| u.kind == UnitKind::Heading))
        .map(|u| u.text.clone());
    let is_scanned = is_pdf
        && page_count.is_some()
        && is_low_text_density(page_count, total_text_chars, pages_with_text.len());
    NormalizedDocument {
        title,
        page_count,
        is_scanned,
        units,
    }
}
