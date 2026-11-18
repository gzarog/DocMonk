//! AI file analysis (ADR 0045): the provider-neutral contract every
//! analysis provider returns, the prompt that asks for it, and its strict
//! validation.
//!
//! A provider receives one file as [`AnalysisInput`] parts (native PDF or
//! image bytes, or text) and must answer with one JSON object matching
//! [`analysis_json_schema`]. [`parse_result`] accepts only a valid object;
//! anything else is [`AnalysisErrorKind::InvalidOutput`]. Nothing here reads
//! storage, chunks text or ranks anything: the document processor maps a
//! validated [`AnalysisResult`] to rows.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// The analysis result schema revision. A change rebuilds every document
/// analysed by AI (it is part of the analysis identity).
pub const ANALYSIS_SCHEMA_VERSION: u32 = 1;

/// Chunk kinds a model may return (the canonical unit kinds).
pub const CHUNK_KINDS: &[&str] = &["heading", "text", "table", "list", "caption"];

/// Code reference kinds a model may return.
pub const REFERENCE_KINDS: &[&str] = &["symbol", "path", "route", "topic"];

/// One file's analysis.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalysisResult {
    pub schema_version: u32,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    /// ISO 639-1 code, when the model can tell.
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Pages of a paged format (PDF), as the model counted them.
    #[serde(default)]
    pub page_count: Option<i64>,
    pub chunks: Vec<AnalysisChunk>,
    #[serde(default)]
    pub code_references: Vec<CodeReference>,
}

/// One retrievable unit, in reading order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalysisChunk {
    pub kind: String,
    #[serde(default)]
    pub heading_path: Vec<String>,
    #[serde(default)]
    pub heading_level: Option<i64>,
    #[serde(default)]
    pub page_start: Option<i64>,
    #[serde(default)]
    pub page_end: Option<i64>,
    /// The file's own words, verbatim.
    pub text: String,
    /// Text to embed: the chunk with its context, may paraphrase.
    #[serde(default)]
    pub embedding_text: Option<String>,
    #[serde(default)]
    pub table_rows: Option<Vec<Vec<String>>>,
}

/// A reference to code the file names (a symbol, path, HTTP route or
/// messaging topic). It only becomes a link when it resolves to an
/// existing code entity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeReference {
    pub kind: String,
    pub value: String,
    #[serde(default)]
    pub chunk_index: Option<usize>,
    #[serde(default)]
    pub quote: Option<String>,
}

/// One piece of a file sent to the model.
#[derive(Debug, Clone, PartialEq)]
pub enum InputPart {
    /// Plain text, optionally named (a member of a container format).
    Text { name: Option<String>, text: String },
    /// A whole PDF, sent natively.
    Pdf { bytes: Vec<u8> },
    /// One image (`image/png`, `image/jpeg`, ...).
    Image { media_type: String, bytes: Vec<u8> },
}

impl InputPart {
    pub fn kind(&self) -> InputKind {
        match self {
            InputPart::Text { .. } => InputKind::Text,
            InputPart::Pdf { .. } => InputKind::Pdf,
            InputPart::Image { .. } => InputKind::Image,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            InputPart::Text { text, .. } => text.len(),
            InputPart::Pdf { bytes } | InputPart::Image { bytes, .. } => bytes.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Text,
    Pdf,
    Image,
}

impl InputKind {
    pub fn as_str(self) -> &'static str {
        match self {
            InputKind::Text => "text",
            InputKind::Pdf => "pdf",
            InputKind::Image => "image",
        }
    }
}

/// One file to analyse.
#[derive(Debug, Clone, PartialEq)]
pub struct AnalysisInput {
    /// The file name shown to the model (never a full path).
    pub file_name: String,
    /// DocMonk's format name (`pdf`, `docx`, ...).
    pub format: String,
    pub parts: Vec<InputPart>,
    /// Pages of a paged format, when known without analysing it.
    pub page_count: Option<i64>,
}

impl AnalysisInput {
    pub fn total_bytes(&self) -> usize {
        self.parts.iter().map(InputPart::len).sum()
    }
}

/// Why an analysis failed; each maps to an `ai_analysis_*` file error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalysisErrorKind {
    /// Usage or rate limit reached (retried on the next index).
    Quota,
    /// No answer within the deadline (retried).
    Timeout,
    /// The provider failed or was unreachable (retried).
    Failed,
    /// Not signed in, or the key was rejected (retried after sign-in).
    NotSignedIn,
    /// The answer is not a valid analysis (permanent until the file changes).
    InvalidOutput,
    /// The provider cannot take this format (permanent).
    UnsupportedFormat,
    /// Over `documents.analysis.max_file_bytes` or a provider limit (permanent).
    TooLarge,
    /// The provider rejected the request itself (HTTP 400/404/422; permanent).
    Rejected,
    /// The model declined to analyse the file (permanent).
    Refused,
}

impl AnalysisErrorKind {
    /// The file error code recorded for this failure.
    pub fn code(self) -> &'static str {
        match self {
            AnalysisErrorKind::Quota => "ai_analysis_quota",
            AnalysisErrorKind::Timeout => "ai_analysis_timeout",
            AnalysisErrorKind::Failed => "ai_analysis_failed",
            AnalysisErrorKind::NotSignedIn => "ai_analysis_not_signed_in",
            AnalysisErrorKind::InvalidOutput => "ai_analysis_invalid_output",
            AnalysisErrorKind::UnsupportedFormat => "ai_analysis_unsupported_format",
            AnalysisErrorKind::TooLarge => "ai_analysis_too_large",
            AnalysisErrorKind::Rejected => "ai_analysis_rejected",
            AnalysisErrorKind::Refused => "ai_analysis_refused",
        }
    }

    /// Retried on the next index pass (decision D5).
    pub fn transient(self) -> bool {
        matches!(
            self,
            AnalysisErrorKind::Quota
                | AnalysisErrorKind::Timeout
                | AnalysisErrorKind::Failed
                | AnalysisErrorKind::NotSignedIn
        )
    }
}

/// Every `ai_analysis_*` code, for status and documentation.
pub const ERROR_CODES: &[&str] = &[
    "ai_analysis_quota",
    "ai_analysis_timeout",
    "ai_analysis_failed",
    "ai_analysis_not_signed_in",
    "ai_analysis_invalid_output",
    "ai_analysis_unsupported_format",
    "ai_analysis_too_large",
    "ai_analysis_rejected",
    "ai_analysis_refused",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisError {
    pub kind: AnalysisErrorKind,
    pub message: String,
}

impl AnalysisError {
    pub fn new(kind: AnalysisErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(AnalysisErrorKind::InvalidOutput, message)
    }
}

impl fmt::Display for AnalysisError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AnalysisError {}

/// Analyses one file. Implementations make exactly one model request (no
/// retries) and never touch storage.
pub trait AiAnalyzer: Send + Sync {
    /// The provider id (`anthropic`, `openai`, `claude_code`, `codex`).
    fn provider_id(&self) -> &'static str;
    /// The model requests are sent to.
    fn model(&self) -> &str;
    /// Input kinds this provider accepts.
    fn accepts(&self, kind: InputKind) -> bool;
    fn analyze(&self, input: &AnalysisInput) -> Result<AnalysisResult, AnalysisError>;

    /// `provider:model`, the provenance stored on every AI-produced row.
    fn extracted_by(&self) -> String {
        format!("ai:{}:{}", self.provider_id(), self.model())
    }
}

/// The fixed system prompt.
pub const SYSTEM_PROMPT: &str = "You are DocMonk's document analyser. You read one file and return \
its complete content as structured JSON for a search index whose answers cite their sources. \
Rules: (1) `text` of every chunk is the file's own words, verbatim, in reading order; never \
summarise, translate, correct or invent text there. (2) Cover the whole file: every heading, \
paragraph, list, table and caption becomes a chunk; skip only page furniture (running headers, \
footers, page numbers). (3) Headings: a `heading` chunk per heading, `heading_level` 1-6, and every \
chunk carries `heading_path`, the titles of its enclosing headings, outermost first. (4) Tables: \
`kind` `table`, `table_rows` the cells row by row with the header row first, and `text` the table \
as plain text. (5) Paged files: `page_start`/`page_end` are 1-based physical pages. (6) \
`embedding_text` may add context (document title, headings) to help retrieval. (7) `summary` is \
at most three sentences; `keywords` at most 12. (8) `code_references` lists only code identifiers, \
file paths, HTTP routes or message topics the file itself names, with the chunk index and a short \
verbatim quote. (9) Answer with the JSON object only.";

/// The per-file instruction.
pub fn instruction(input: &AnalysisInput) -> String {
    let pages = input
        .page_count
        .map(|n| format!(" It has {n} page(s)."))
        .unwrap_or_default();
    format!(
        "Analyse the attached file `{}` (format: {}).{pages} Return one JSON object that matches \
         this JSON Schema exactly, with schema_version {ANALYSIS_SCHEMA_VERSION}:\n{}",
        input.file_name,
        input.format,
        analysis_json_schema()
    )
}

/// The JSON Schema of [`AnalysisResult`] (providers that support
/// structured output receive it as such).
pub fn analysis_json_schema() -> Value {
    let nullable_int = json!({"type": ["integer", "null"]});
    let nullable_str = json!({"type": ["string", "null"]});
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["schema_version", "title", "author", "language", "summary", "keywords",
                     "page_count", "chunks", "code_references"],
        "properties": {
            "schema_version": {"type": "integer", "enum": [ANALYSIS_SCHEMA_VERSION]},
            "title": nullable_str,
            "author": nullable_str,
            "language": nullable_str,
            "summary": nullable_str,
            "keywords": {"type": "array", "items": {"type": "string"}},
            "page_count": nullable_int,
            "chunks": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["kind", "heading_path", "heading_level", "page_start", "page_end",
                                 "text", "embedding_text", "table_rows"],
                    "properties": {
                        "kind": {"type": "string", "enum": CHUNK_KINDS},
                        "heading_path": {"type": "array", "items": {"type": "string"}},
                        "heading_level": nullable_int,
                        "page_start": nullable_int,
                        "page_end": nullable_int,
                        "text": {"type": "string"},
                        "embedding_text": nullable_str,
                        "table_rows": {
                            "type": ["array", "null"],
                            "items": {"type": "array", "items": {"type": "string"}}
                        }
                    }
                }
            },
            "code_references": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["kind", "value", "chunk_index", "quote"],
                    "properties": {
                        "kind": {"type": "string", "enum": REFERENCE_KINDS},
                        "value": {"type": "string"},
                        "chunk_index": nullable_int,
                        "quote": nullable_str
                    }
                }
            }
        }
    })
}

/// The analysis identity: schema revision plus a hash of the prompt and
/// schema. Part of the converter identity, so a prompt change rebuilds.
pub fn analysis_identity() -> String {
    let mut h = Sha256::new();
    h.update(SYSTEM_PROMPT.as_bytes());
    h.update(analysis_json_schema().to_string().as_bytes());
    let digest = format!("{:x}", h.finalize());
    format!("schema.{ANALYSIS_SCHEMA_VERSION}+prompt.{}", &digest[..12])
}

/// The JSON object inside a model's text answer: the whole text, or the
/// body of a single fenced block.
fn json_text(text: &str) -> &str {
    let t = text.trim();
    if let Some(rest) = t.strip_prefix("```") {
        let rest = rest.strip_prefix("json").unwrap_or(rest);
        if let Some(body) = rest.strip_suffix("```") {
            return body.trim();
        }
    }
    t
}

/// Parses and validates a model's text answer.
pub fn parse_result(text: &str, page_count: Option<i64>) -> Result<AnalysisResult, AnalysisError> {
    let value: Value = serde_json::from_str(json_text(text))
        .map_err(|e| AnalysisError::invalid(format!("the answer is not JSON: {e}")))?;
    validate_value(value, page_count)
}

/// Validates an already-decoded JSON value (structured-output providers).
pub fn validate_value(
    value: Value,
    page_count: Option<i64>,
) -> Result<AnalysisResult, AnalysisError> {
    let r: AnalysisResult = serde_json::from_value(value).map_err(|e| {
        AnalysisError::invalid(format!("the answer does not match the schema: {e}"))
    })?;
    validate(&r, page_count)?;
    Ok(r)
}

fn validate(r: &AnalysisResult, page_count: Option<i64>) -> Result<(), AnalysisError> {
    let bad = |m: String| Err(AnalysisError::invalid(m));
    if r.schema_version != ANALYSIS_SCHEMA_VERSION {
        return bad(format!(
            "schema_version {} is not {ANALYSIS_SCHEMA_VERSION}",
            r.schema_version
        ));
    }
    if r.chunks.is_empty() {
        return bad("the analysis has no chunks".into());
    }
    let pages = page_count.or(r.page_count);
    for (i, c) in r.chunks.iter().enumerate() {
        if !CHUNK_KINDS.contains(&c.kind.as_str()) {
            return bad(format!("chunk {i}: unknown kind {:?}", c.kind));
        }
        if c.text.trim().is_empty() {
            return bad(format!("chunk {i}: empty text"));
        }
        if let Some(l) = c.heading_level {
            if !(0..=6).contains(&l) {
                return bad(format!("chunk {i}: heading_level {l} is not 0-6"));
            }
        }
        match (c.page_start, c.page_end) {
            (Some(s), Some(e)) if s > e => {
                return bad(format!("chunk {i}: page_start {s} > page_end {e}"))
            }
            _ => {}
        }
        for p in [c.page_start, c.page_end].into_iter().flatten() {
            let max = pages.unwrap_or(i64::MAX);
            if p < 1 || p > max {
                return bad(format!("chunk {i}: page {p} is outside 1..={max}"));
            }
        }
        if c.kind == "table" && c.table_rows.as_ref().is_none_or(Vec::is_empty) {
            return bad(format!("chunk {i}: a table chunk needs table_rows"));
        }
    }
    for (i, r2) in r.code_references.iter().enumerate() {
        if !REFERENCE_KINDS.contains(&r2.kind.as_str()) {
            return bad(format!("code reference {i}: unknown kind {:?}", r2.kind));
        }
        if r2.value.trim().is_empty() {
            return bad(format!("code reference {i}: empty value"));
        }
        if let Some(ci) = r2.chunk_index {
            if ci >= r.chunks.len() {
                return bad(format!(
                    "code reference {i}: chunk_index {ci} is past the last chunk"
                ));
            }
        }
    }
    Ok(())
}

/// Standard base64 (RFC 4648, padded), for inline file data.
pub fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good() -> Value {
        json!({
            "schema_version": 1, "title": "Policy", "author": null, "language": "en",
            "summary": "A policy.", "keywords": ["policy"], "page_count": 2,
            "chunks": [
                {"kind": "heading", "heading_path": [], "heading_level": 1, "page_start": 1,
                 "page_end": 1, "text": "Policy", "embedding_text": null, "table_rows": null},
                {"kind": "table", "heading_path": ["Policy"], "heading_level": null, "page_start": 1,
                 "page_end": 2, "text": "a b", "embedding_text": "Policy: a b",
                 "table_rows": [["a"], ["b"]]}
            ],
            "code_references": [{"kind": "symbol", "value": "PolicyService", "chunk_index": 1,
                                 "quote": "a b"}]
        })
    }

    #[test]
    fn a_valid_answer_parses_from_plain_or_fenced_text() {
        let plain = parse_result(&good().to_string(), Some(2)).unwrap();
        let fenced = parse_result(&format!("```json\n{}\n```", good()), Some(2)).unwrap();
        assert_eq!(plain, fenced);
        assert_eq!(plain.chunks.len(), 2);
        assert_eq!(plain.code_references[0].value, "PolicyService");
    }

    #[test]
    fn invalid_answers_are_rejected_with_a_reason() {
        type Mutation = Box<dyn Fn(&mut Value)>;
        let cases: Vec<(Mutation, &str)> = vec![
            (
                Box::new(|v| v["schema_version"] = json!(2)),
                "schema_version",
            ),
            (Box::new(|v| v["chunks"] = json!([])), "no chunks"),
            (
                Box::new(|v| v["chunks"][0]["kind"] = json!("blob")),
                "unknown kind",
            ),
            (
                Box::new(|v| v["chunks"][0]["text"] = json!("  ")),
                "empty text",
            ),
            (
                Box::new(|v| v["chunks"][1]["page_end"] = json!(3)),
                "outside",
            ),
            (
                Box::new(|v| v["chunks"][1]["page_start"] = json!(3)),
                "> page_end",
            ),
            (
                Box::new(|v| v["chunks"][1]["page_start"] = json!(0)),
                "outside",
            ),
            (
                Box::new(|v| v["chunks"][1]["table_rows"] = json!(null)),
                "table_rows",
            ),
            (
                Box::new(|v| v["chunks"][0]["heading_level"] = json!(9)),
                "heading_level",
            ),
            (
                Box::new(|v| v["code_references"][0]["chunk_index"] = json!(5)),
                "past the last",
            ),
            (Box::new(|v| v["extra"] = json!(1)), "does not match"),
        ];
        for (mutate, want) in cases {
            let mut v = good();
            mutate(&mut v);
            let err = validate_value(v, Some(2)).unwrap_err();
            assert_eq!(err.kind, AnalysisErrorKind::InvalidOutput);
            assert!(err.message.contains(want), "{} !~ {want}", err.message);
        }
        let err = parse_result("not json", None).unwrap_err();
        assert!(err.message.contains("not JSON"));
    }

    #[test]
    fn page_range_is_checked_against_the_known_page_count() {
        // The file's own page count wins over the model's.
        let mut v = good();
        v["page_count"] = json!(10);
        assert!(validate_value(v.clone(), Some(1)).is_err());
        assert!(validate_value(v, None).is_ok());
    }

    #[test]
    fn error_kinds_map_to_codes_and_retry_policy() {
        let all = [
            AnalysisErrorKind::Quota,
            AnalysisErrorKind::Timeout,
            AnalysisErrorKind::Failed,
            AnalysisErrorKind::NotSignedIn,
            AnalysisErrorKind::InvalidOutput,
            AnalysisErrorKind::UnsupportedFormat,
            AnalysisErrorKind::TooLarge,
            AnalysisErrorKind::Rejected,
            AnalysisErrorKind::Refused,
        ];
        let codes: Vec<_> = all.iter().map(|k| k.code()).collect();
        assert_eq!(codes, ERROR_CODES);
        let transient: Vec<_> = all
            .iter()
            .filter(|k| k.transient())
            .map(|k| k.code())
            .collect();
        assert_eq!(
            transient,
            [
                "ai_analysis_quota",
                "ai_analysis_timeout",
                "ai_analysis_failed",
                "ai_analysis_not_signed_in"
            ]
        );
    }

    #[test]
    fn identity_is_stable_and_names_the_schema() {
        assert_eq!(analysis_identity(), analysis_identity());
        assert!(analysis_identity().starts_with("schema.1+prompt."));
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (i, o) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(i.as_bytes()), o);
        }
    }
}
