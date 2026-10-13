//! The canonical intermediate format: a normalized document as a flat,
//! index-addressed unit list, and the
//! chunks produced from it. Field names are the canonical JSON keys.

use serde::{Deserialize, Serialize};

use crate::table::Row;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitKind {
    Heading,
    Paragraph,
    Table,
}

impl UnitKind {
    pub fn as_str(self) -> &'static str {
        match self {
            UnitKind::Heading => "heading",
            UnitKind::Paragraph => "paragraph",
            UnitKind::Table => "table",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedUnit {
    pub kind: UnitKind,
    pub text: String,
    pub heading_level: Option<i64>,
    pub heading_path: Vec<String>,
    pub parent_index: Option<usize>,
    pub page_start: Option<i64>,
    pub page_end: Option<i64>,
    #[serde(default)]
    pub table_rows: Option<Vec<Row>>,
    #[serde(default)]
    pub caption: Option<String>,
    #[serde(default)]
    pub header_row_count: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedDocument {
    pub title: Option<String>,
    pub page_count: Option<i64>,
    pub is_scanned: bool,
    #[serde(default)]
    pub units: Vec<NormalizedUnit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    pub kind: UnitKind,
    pub text: String,
    pub heading_level: Option<i64>,
    pub heading_path: Vec<String>,
    /// Index into the returned chunk list.
    pub parent_index: Option<usize>,
    pub page_start: Option<i64>,
    pub page_end: Option<i64>,
    pub table_rows: Option<Vec<Row>>,
    pub caption: Option<String>,
    /// Embedding input: fitted breadcrumb header + body.
    pub contextual_text: String,
    /// FTS input: title, heading path segments, body (one per line).
    pub search_text: String,
    /// Exact body tokens of the text the chunk was counted from.
    pub token_count: i64,
}

/// Document-level metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentMetadata {
    pub title: Option<String>,
    pub author: Option<String>,
    pub page_count: Option<i64>,
    pub format: String,
    pub is_scanned: bool,
    pub source_filename: String,
}

impl DocumentMetadata {
    /// The normalized title, else the file stem (author is never guessed).
    pub fn from_normalized(doc: &NormalizedDocument, format: &str, file_name: &str) -> Self {
        let stem = match crate::model::stem(file_name) {
            "" => file_name,
            s => s,
        };
        Self {
            title: Some(
                doc.title
                    .clone()
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| stem.to_owned()),
            ),
            author: None,
            page_count: doc.page_count,
            format: format.to_owned(),
            is_scanned: doc.is_scanned,
            source_filename: file_name.to_owned(),
        }
    }
}

/// `PurePath(name).stem`.
pub fn stem(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => &name[..i],
        _ => name,
    }
}
