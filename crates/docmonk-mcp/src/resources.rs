//! MCP resources (ADR 0045): a client attaches a document, a chunk or a
//! symbol to its context by URI instead of calling a tool.
//!
//! | URI | Content |
//! |---|---|
//! | `docmonk://source/{source_id}` | The source's id, path and published build (listed by `resources/list`) |
//! | `docmonk://document/{source_id}/{document_id}{?from}` | A document's chunks in order, starting at ordinal `from` |
//! | `docmonk://chunk/{source_id}/{chunk_id}` | One document chunk |
//! | `docmonk://symbol/{source_id}/{entity_id}` | A code symbol: signature, location and body |
//!
//! The ids are the ones tool results carry (`source_id`, an evidence
//! entry's `record_id`, a link's `document_id`). Every read goes through
//! the `KnowledgeRead` port against the source's published build, inside
//! the same visibility scope, executor limits and query log as a tool
//! call (see `run_tool` in the crate root). Stored text is already
//! redacted (ADR 0042).

use docmonk_config::DocMonkConfig;
use docmonk_core::errors::DocMonkError;
use docmonk_core::paths::Home;
use serde_json::{json, Value};

use docmonk_service::query::{join_location, open_sources, search_err, Opened};

pub const SCHEME: &str = "docmonk://";

/// What a `docmonk://` URI names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Source {
        source_id: String,
    },
    Document {
        source_id: String,
        document_id: String,
        /// Ordinal of the first chunk to return.
        from: i64,
    },
    Chunk {
        source_id: String,
        chunk_id: String,
    },
    Symbol {
        source_id: String,
        entity_id: String,
    },
}

impl Target {
    pub fn source_id(&self) -> &str {
        match self {
            Self::Source { source_id }
            | Self::Document { source_id, .. }
            | Self::Chunk { source_id, .. }
            | Self::Symbol { source_id, .. } => source_id,
        }
    }
}

/// The resource templates `resources/templates/list` returns.
pub fn templates() -> Value {
    json!([
        {
            "uriTemplate": "docmonk://document/{source_id}/{document_id}{?from}",
            "name": "document",
            "title": "Indexed document",
            "description": "The text of an indexed document, chunk by chunk in reading order. \
                A long document is cut at the result budget and ends with the URI that \
                continues it (`from` is the ordinal of the first chunk returned).",
            "mimeType": "text/plain",
        },
        {
            "uriTemplate": "docmonk://chunk/{source_id}/{chunk_id}",
            "name": "chunk",
            "title": "Document chunk",
            "description": "One chunk of an indexed document with its heading path and pages. \
                `chunk_id` is the `record_id` of a document evidence entry (for a title \
                hit, the document's first chunk).",
            "mimeType": "text/plain",
        },
        {
            "uriTemplate": "docmonk://symbol/{source_id}/{entity_id}",
            "name": "symbol",
            "title": "Code symbol",
            "description": "A code symbol's qualified name, signature, file, lines and body. \
                `entity_id` is the `entity_id` of a docmonk_explore entity or the `record_id` of an \
                entity evidence entry.",
            "mimeType": "text/plain",
        },
    ])
}

fn not_found(uri: &str) -> DocMonkError {
    DocMonkError::usage(format!("no such resource: {uri}"))
}

/// Percent-decodes one path segment; `None` for a malformed escape or a
/// decoded segment that is empty or not UTF-8.
fn segment(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = raw.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok().filter(|s| !s.is_empty())
}

/// Parses a `docmonk://` URI. Anything else, including an unknown kind,
/// a missing id or a malformed `from`, names no resource.
pub fn parse(uri: &str) -> Result<Target, DocMonkError> {
    let bad = || not_found(uri);
    let rest = uri.strip_prefix(SCHEME).ok_or_else(bad)?;
    if rest.contains('#') {
        return Err(bad());
    }
    let (path, query) = match rest.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (rest, None),
    };
    let parts: Vec<&str> = path.split('/').collect();
    let id = |i: usize| parts.get(i).and_then(|p| segment(p)).ok_or_else(bad);
    let target = match (parts[0], parts.len()) {
        ("source", 2) => Target::Source { source_id: id(1)? },
        ("document", 3) => {
            let from = match query {
                None => 0,
                Some(q) => q
                    .strip_prefix("from=")
                    .and_then(|n| n.parse::<i64>().ok())
                    .filter(|n| *n >= 0)
                    .ok_or_else(bad)?,
            };
            return Ok(Target::Document {
                source_id: id(1)?,
                document_id: id(2)?,
                from,
            });
        }
        ("chunk", 3) => Target::Chunk {
            source_id: id(1)?,
            chunk_id: id(2)?,
        },
        ("symbol", 3) => Target::Symbol {
            source_id: id(1)?,
            entity_id: id(2)?,
        },
        _ => return Err(bad()),
    };
    // Only a document takes a query.
    if query.is_some() {
        return Err(bad());
    }
    Ok(target)
}

/// Percent-encodes an id for use as one URI path segment.
fn encode(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for b in id.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn source_uri(source_id: &str) -> String {
    format!("{SCHEME}source/{}", encode(source_id))
}

pub fn document_uri(source_id: &str, document_id: &str, from: i64) -> String {
    let base = format!(
        "{SCHEME}document/{}/{}",
        encode(source_id),
        encode(document_id)
    );
    if from > 0 {
        format!("{base}?from={from}")
    } else {
        base
    }
}

/// `resources/list`: one resource per source with a published build that
/// the caller may see. Each entry carries its `source_id` and `path` so
/// the visibility scrub and the query log treat it like a tool record.
pub fn list(home: &Home) -> Result<Value, DocMonkError> {
    let resources: Vec<Value> = open_sources(home, None)?
        .iter()
        .map(|o| {
            let name = std::path::Path::new(&o.source.path)
                .file_name()
                .map_or_else(|| o.source.id.clone(), |n| n.to_string_lossy().into_owned());
            json!({
                "uri": source_uri(&o.source.id),
                "name": name,
                "title": format!("Source {name}"),
                "description": format!("Indexed source at {}.", o.source.path),
                "mimeType": "application/json",
                "source_id": o.source.id,
                "path": o.source.path,
            })
        })
        .collect();
    Ok(json!({ "resources": resources }))
}

/// The largest text one read returns: three quarters of
/// `mcp.max_result_bytes`, leaving room for JSON escaping.
fn text_budget(cfg: &DocMonkConfig) -> usize {
    usize::try_from(cfg.mcp.max_result_bytes).unwrap_or(262_144) / 4 * 3
}

/// Cuts `text` to `budget` bytes on a character boundary, saying so.
fn clip(mut text: String, budget: usize) -> String {
    if text.len() <= budget {
        return text;
    }
    let mut end = budget;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("\n[truncated at mcp.max_result_bytes]\n");
    text
}

fn pages(start: Option<i64>, end: Option<i64>) -> String {
    match (start, end) {
        (Some(s), Some(e)) if e != s => format!(", pages {s}-{e}"),
        (Some(s), _) => format!(", page {s}"),
        _ => String::new(),
    }
}

fn file_path(o: &Opened, file_id: &str) -> Result<String, DocMonkError> {
    let rel = o
        .store
        .read()
        .file_rel_path(&o.build, file_id)
        .map_err(|e| search_err(e.into()))?
        .unwrap_or_default();
    Ok(join_location(&o.source.path, &rel))
}

fn read_source(o: &Opened) -> (String, &'static str) {
    let body = json!({
        "source_id": o.source.id,
        "path": o.source.path,
        "build_id": o.build,
        "backend": o.store.read().backend_kind(),
        "read_with": {
            "document": "docmonk://document/{source_id}/{document_id}",
            "chunk": "docmonk://chunk/{source_id}/{chunk_id}",
            "symbol": "docmonk://symbol/{source_id}/{entity_id}",
        },
    });
    (
        serde_json::to_string_pretty(&body).unwrap_or_default(),
        "application/json",
    )
}

fn read_chunk(o: &Opened, uri: &str, chunk_id: &str) -> Result<String, DocMonkError> {
    let read = o.store.read();
    let chunk = match read
        .chunk(&o.build, chunk_id)
        .map_err(|e| search_err(e.into()))?
    {
        Some(chunk) => chunk,
        // A title hit cites the document itself: its first chunk, as
        // citation verification reads it.
        None => read
            .chunk_at(&o.build, chunk_id, 0)
            .map_err(|e| search_err(e.into()))?
            .ok_or_else(|| not_found(uri))?,
    };
    let mut text = format!(
        "{}\nchunk {} of {} (build {}){}\n",
        file_path(o, &chunk.file_id)?,
        chunk.ordinal,
        document_uri(&o.source.id, &chunk.document_id, 0),
        o.build,
        pages(chunk.page_start, chunk.page_end),
    );
    if !chunk.heading_path.is_empty() {
        text.push_str(&format!("{}\n", chunk.heading_path.join(" > ")));
    }
    text.push('\n');
    text.push_str(&chunk.text);
    text.push('\n');
    Ok(text)
}

fn read_document(
    o: &Opened,
    uri: &str,
    document_id: &str,
    from: i64,
    budget: usize,
) -> Result<String, DocMonkError> {
    let read = o.store.read();
    let first = read
        .chunk_at(&o.build, document_id, from)
        .map_err(|e| search_err(e.into()))?
        .ok_or_else(|| not_found(uri))?;
    let mut text = format!(
        "{}\ndocument {document_id} (build {}), from chunk {from}\n",
        file_path(o, &first.file_id)?,
        o.build
    );
    let mut next = Some(first);
    while let Some(chunk) = next {
        // Always return at least one chunk, so a continuation advances.
        if chunk.ordinal > from && text.len() + chunk.text.len() > budget {
            text.push_str(&format!(
                "\n[continues at {}]\n",
                document_uri(&o.source.id, document_id, chunk.ordinal)
            ));
            break;
        }
        text.push('\n');
        text.push_str(&chunk.text);
        text.push('\n');
        next = read
            .chunk_at(&o.build, document_id, chunk.ordinal + 1)
            .map_err(|e| search_err(e.into()))?;
    }
    Ok(text)
}

fn read_symbol(o: &Opened, uri: &str, entity_id: &str) -> Result<String, DocMonkError> {
    let read = o.store.read();
    let entity = read
        .entity(&o.build, entity_id)
        .map_err(|e| search_err(e.into()))?
        .ok_or_else(|| not_found(uri))?;
    let mut text = format!(
        "{} ({} {})\n{}:{}-{} (build {})\n",
        entity.qualified_name,
        entity.language,
        entity.kind,
        file_path(o, &entity.file_id)?,
        entity.start_line,
        entity.end_line,
        o.build,
    );
    if let Some(signature) = entity.signature.as_deref().filter(|s| !s.is_empty()) {
        text.push_str(&format!("{signature}\n"));
    }
    let passages = read
        .entity_passages(&o.build, entity_id)
        .map_err(|e| search_err(e.into()))?;
    for passage in &passages {
        text.push_str(&format!(
            "\nlines {}-{}:\n{}\n",
            passage.start_line, passage.end_line, passage.text
        ));
    }
    Ok(text)
}

/// `resources/read`: `{"contents": [{uri, mimeType, text, source_id}]}`.
/// A URI that names nothing readable (unknown or hidden source, unknown
/// record, malformed URI) is a usage error, reported to the client as
/// "resource not found".
pub fn read(home: &Home, cfg: &DocMonkConfig, uri: &str) -> Result<Value, DocMonkError> {
    let target = parse(uri)?;
    let opened = open_sources(home, Some(target.source_id())).map_err(|_| not_found(uri))?;
    // A source without a published build has nothing to read.
    let o = opened.first().ok_or_else(|| not_found(uri))?;
    let budget = text_budget(cfg);
    let (text, mime) = match &target {
        Target::Source { .. } => read_source(o),
        Target::Chunk { chunk_id, .. } => (read_chunk(o, uri, chunk_id)?, "text/plain"),
        Target::Document {
            document_id, from, ..
        } => (
            read_document(o, uri, document_id, *from, budget)?,
            "text/plain",
        ),
        Target::Symbol { entity_id, .. } => (read_symbol(o, uri, entity_id)?, "text/plain"),
    };
    Ok(json!({
        "contents": [{
            "uri": uri,
            "mimeType": mime,
            "text": clip(text, budget),
            "source_id": o.source.id,
        }],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_parse_to_their_targets() {
        assert_eq!(
            parse("docmonk://source/src_1").unwrap(),
            Target::Source {
                source_id: "src_1".into()
            }
        );
        assert_eq!(
            parse("docmonk://document/src_1/doc%3A9?from=12").unwrap(),
            Target::Document {
                source_id: "src_1".into(),
                document_id: "doc:9".into(),
                from: 12
            }
        );
        assert_eq!(
            parse("docmonk://chunk/s/c").unwrap(),
            Target::Chunk {
                source_id: "s".into(),
                chunk_id: "c".into()
            }
        );
        assert_eq!(
            parse("docmonk://symbol/s/e").unwrap(),
            Target::Symbol {
                source_id: "s".into(),
                entity_id: "e".into()
            }
        );
    }

    #[test]
    fn anything_else_names_no_resource() {
        for uri in [
            "",
            "file:///etc/passwd",
            "docmonk://",
            "docmonk://source",
            "docmonk://source/",
            "docmonk://source/a/b",
            "docmonk://source/a?from=1",
            "docmonk://document/a",
            "docmonk://document/a/b/c",
            "docmonk://document/a/b?from=-1",
            "docmonk://document/a/b?from=x",
            "docmonk://document/a/b?other=1",
            "docmonk://chunk/a/b?from=1",
            "docmonk://chunk/a/%zz",
            "docmonk://chunk/a/%4",
            "docmonk://symbol/a/b#frag",
            "docmonk://file/a/b",
        ] {
            let e = parse(uri).expect_err(uri);
            assert!(e.message().starts_with("no such resource"), "{uri}");
        }
    }

    #[test]
    fn ids_round_trip_through_a_uri() {
        let uri = document_uri("src 1", "doc/9?x", 3);
        assert_eq!(uri, "docmonk://document/src%201/doc%2F9%3Fx?from=3");
        assert_eq!(
            parse(&uri).unwrap(),
            Target::Document {
                source_id: "src 1".into(),
                document_id: "doc/9?x".into(),
                from: 3
            }
        );
    }

    #[test]
    fn long_text_is_cut_on_a_character_boundary() {
        let out = clip("ééééé".into(), 5);
        assert!(out.starts_with("éé\n[truncated"), "{out}");
        assert_eq!(clip("short".into(), 5), "short");
    }
}
