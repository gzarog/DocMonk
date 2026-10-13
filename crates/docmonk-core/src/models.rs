//! Domain models. Enum wire values and struct
//! field names are persisted in SQLite/server indexes and emitted in CLI
//! JSON, so they must not change.

use serde::{Deserialize, Serialize};

macro_rules! str_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub enum $name {
            $(#[serde(rename = $value)] $variant),+
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub const fn as_str(self) -> &'static str {
                match self { $($name::$variant => $value),+ }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for $name {
            type Err = UnknownVariant;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($value => Ok($name::$variant),)+
                    other => Err(UnknownVariant { kind: stringify!($name), value: other.to_owned() }),
                }
            }
        }
    };
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown {kind} value {value:?}")]
pub struct UnknownVariant {
    pub kind: &'static str,
    pub value: String,
}

str_enum!(SourceType { Local => "local", Network => "network" });
str_enum!(SourceStatus { Active => "active", Offline => "offline" });
str_enum!(IndexingMode { Full => "full" });
str_enum!(FileKind { Code => "code", Document => "document", Unknown => "unknown" });
str_enum!(FileStatus {
    Discovered => "discovered",
    Classified => "classified",
    Queued => "queued",
    Processing => "processing",
    Retry => "retry",
    Failed => "failed",
    Indexed => "indexed",
    SkippedLimit => "skipped_limit",
});
str_enum!(JobStatus {
    Queued => "queued",
    Processing => "processing",
    Retry => "retry",
    Failed => "failed",
    Completed => "completed",
});
str_enum!(JobType { IndexFile => "index_file" });
str_enum!(EntityType {
    Namespace => "namespace",
    Class => "class",
    Interface => "interface",
    Struct => "struct",
    Enum => "enum",
    Function => "function",
    Method => "method",
    Property => "property",
    Field => "field",
});
str_enum!(RelationshipType {
    Calls => "calls",
    Implements => "implements",
    Extends => "extends",
    Imports => "imports",
    References => "references",
    Contains => "contains",
    DefinedIn => "defined_in",
    DocumentedBy => "documented_by",
    MentionedIn => "mentioned_in",
    RelatedTo => "related_to",
});
str_enum!(Confidence { Exact => "exact", High => "high", Medium => "medium", Heuristic => "heuristic" });
str_enum!(DocumentFormat {
    Pdf => "pdf",
    Docx => "docx",
    Pptx => "pptx",
    Xlsx => "xlsx",
    Html => "html",
    Markdown => "markdown",
    Txt => "txt",
    Eml => "eml",
    Csv => "csv",
    Odt => "odt",
    Ods => "ods",
    Odp => "odp",
    Epub => "epub",
    Image => "image",
});
str_enum!(SectionKind { Heading => "heading", Paragraph => "paragraph", Table => "table" });
str_enum!(EmbeddingSubjectType { Entity => "entity", DocumentSection => "document_section" });

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub id: String,
    pub path: String,
    pub source_type: SourceType,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "full")]
    pub indexing_mode: IndexingMode,
    #[serde(default)]
    pub include_patterns: Vec<String>,
    #[serde(default)]
    pub exclude_patterns: Vec<String>,
    #[serde(default)]
    pub last_scan_at: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub fingerprint: Option<String>,
    #[serde(default = "active")]
    pub status: SourceStatus,
    pub created_at: String,
    pub updated_at: String,
}

fn yes() -> bool {
    true
}
fn full() -> IndexingMode {
    IndexingMode::Full
}
fn active() -> SourceStatus {
    SourceStatus::Active
}
fn parser_default() -> String {
    "1".into()
}
fn heading() -> SectionKind {
    SectionKind::Heading
}
fn paragraph() -> SectionKind {
    SectionKind::Paragraph
}
fn table() -> SectionKind {
    SectionKind::Table
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileRecord {
    pub id: String,
    pub source_id: String,
    pub path: String,
    pub kind: FileKind,
    pub size: i64,
    pub mtime: f64,
    #[serde(default)]
    pub content_hash: Option<String>,
    pub status: FileStatus,
    #[serde(default)]
    pub generation: i64,
    #[serde(default = "parser_default")]
    pub parser_version: String,
    #[serde(default)]
    pub chunker_version: Option<String>,
    #[serde(default)]
    pub embedding_model_id: Option<String>,
    #[serde(default)]
    pub embedding_text_version: Option<String>,
    #[serde(default)]
    pub last_indexed_at: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexJob {
    pub id: String,
    pub source_id: String,
    pub file_id: String,
    pub job_type: JobType,
    pub status: JobStatus,
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub attempt_count: i64,
    #[serde(default)]
    pub next_attempt_at: Option<String>,
    pub created_at: String,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub completed_at: Option<String>,
    #[serde(default)]
    pub error_code: Option<String>,
    #[serde(default)]
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexErrorRecord {
    pub id: String,
    pub source_id: String,
    #[serde(default)]
    pub file_id: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    pub error_code: String,
    pub error_message: String,
    pub occurred_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScannedFile {
    pub path: String,
    pub size: i64,
    pub mtime: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entity {
    pub id: String,
    pub source_id: String,
    pub file_id: String,
    pub kind: EntityType,
    pub name: String,
    pub qualified_name: String,
    pub language: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub signature: Option<String>,
    pub start_line: i64,
    pub end_line: i64,
    #[serde(default)]
    pub start_col: i64,
    #[serde(default)]
    pub end_col: i64,
    pub generation: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Relationship {
    pub id: String,
    pub relationship_type: RelationshipType,
    pub source_entity_id: String,
    #[serde(default)]
    pub target_entity_id: Option<String>,
    #[serde(default)]
    pub target_symbol: Option<String>,
    pub resolver: String,
    pub confidence: Confidence,
    pub file_id: String,
    #[serde(default)]
    pub source_location: Option<String>,
    #[serde(default)]
    pub evidence: Option<String>,
    pub generation: i64,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrossLink {
    pub id: String,
    pub link_type: RelationshipType,
    pub entity_id: String,
    pub document_id: String,
    #[serde(default)]
    pub section_id: Option<String>,
    pub resolver: String,
    pub confidence: Confidence,
    #[serde(default)]
    pub evidence: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Document {
    pub id: String,
    pub source_id: String,
    pub file_id: String,
    pub format: DocumentFormat,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub page_count: Option<i64>,
    #[serde(default)]
    pub section_count: i64,
    #[serde(default)]
    pub paragraph_count: i64,
    #[serde(default)]
    pub table_count: i64,
    #[serde(default)]
    pub is_scanned: bool,
    #[serde(default)]
    pub content_hash: Option<String>,
    pub generation: i64,
    pub created_at: String,
    pub updated_at: String,
    /// EML attachment provenance: set only on attachment child documents.
    #[serde(default)]
    pub parent_document_id: Option<String>,
    #[serde(default)]
    pub attachment_name: Option<String>,
    #[serde(default)]
    pub attachment_content_type: Option<String>,
    #[serde(default)]
    pub attachment_index: Option<i64>,
    #[serde(default)]
    pub attachment_content_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Section {
    pub id: String,
    pub document_id: String,
    pub file_id: String,
    #[serde(default = "heading")]
    pub kind: SectionKind,
    pub heading_level: i64,
    pub text: String,
    #[serde(default)]
    pub heading_path: Vec<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    pub order_index: i64,
    #[serde(default)]
    pub page_start: Option<i64>,
    #[serde(default)]
    pub page_end: Option<i64>,
    pub generation: i64,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Paragraph {
    pub id: String,
    pub document_id: String,
    pub file_id: String,
    #[serde(default = "paragraph")]
    pub kind: SectionKind,
    pub text: String,
    #[serde(default)]
    pub heading_path: Vec<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    pub order_index: i64,
    #[serde(default)]
    pub page_start: Option<i64>,
    #[serde(default)]
    pub page_end: Option<i64>,
    pub generation: i64,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Table {
    pub id: String,
    pub document_id: String,
    pub file_id: String,
    #[serde(default = "table")]
    pub kind: SectionKind,
    #[serde(default)]
    pub heading_path: Vec<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    pub rows: Vec<Vec<String>>,
    pub num_rows: i64,
    pub num_cols: i64,
    pub order_index: i64,
    #[serde(default)]
    pub page_start: Option<i64>,
    #[serde(default)]
    pub page_end: Option<i64>,
    #[serde(default)]
    pub caption: Option<String>,
    pub generation: i64,
    pub created_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_wire_values_round_trip() {
        for v in FileStatus::ALL {
            assert_eq!(v.as_str().parse::<FileStatus>().unwrap(), *v);
            assert_eq!(serde_json::to_string(v).unwrap(), format!("\"{v}\""));
        }
        assert_eq!(RelationshipType::DefinedIn.as_str(), "defined_in");
        assert!("bogus".parse::<Confidence>().is_err());
    }

    #[test]
    fn source_defaults_when_fields_omitted() {
        let s: Source = serde_json::from_str(
            r#"{"id":"src_1","path":"/x","source_type":"local","created_at":"t","updated_at":"t"}"#,
        )
        .unwrap();
        assert!(s.enabled);
        assert_eq!(s.indexing_mode, IndexingMode::Full);
        assert_eq!(s.status, SourceStatus::Active);
        let doc: Document = serde_json::from_str(
            r#"{"id":"d","source_id":"s","file_id":"f","format":"eml","generation":1,"created_at":"t","updated_at":"t"}"#,
        )
        .unwrap();
        assert_eq!(doc.parent_document_id, None);
        assert!(!doc.is_scanned);
    }
}
