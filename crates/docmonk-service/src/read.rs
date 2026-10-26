//! Reading a cited span (`docmonk_read`, roadmap P8): an agent hands back
//! a citation from `evidence` (or `ask`) and gets the stored text it
//! stands for, with context, after the citation is re-verified against
//! the current snapshot (ADR 0033). A citation whose source, build,
//! record, location or content no longer matches is refused with the
//! reason, never answered from stale text.

use docmonk_config::DocMonkConfig;
use docmonk_core::errors::DocMonkError;
use docmonk_core::paths::Home;
use docmonk_retrieval::context::{expand_chunk_context, ContextOptions};
use docmonk_retrieval::grounding::{self, EvidenceRef};
use docmonk_retrieval::route::SearchFilters;
use serde_json::{json, Value};

use crate::query::{corpora, join_location, open_sources, search_err};

/// Lines of code shown around a cited span, at most.
pub const MAX_CONTEXT_LINES: usize = 200;

/// The citation's code text, numbered, with the cited lines marked `>`.
fn numbered(text: &str, first: i64, cited: Option<(i64, i64)>, context: usize) -> (String, bool) {
    let lines: Vec<&str> = text.lines().collect();
    let last = first + lines.len() as i64 - 1;
    let (from, to) = match cited {
        Some((s, e)) => (
            (s - context as i64).max(first),
            (e + context as i64).min(last),
        ),
        None => (first, last),
    };
    let mut out = String::new();
    for (i, l) in lines.iter().enumerate() {
        let n = first + i as i64;
        if n < from || n > to {
            continue;
        }
        let mark = if cited.is_some_and(|(s, e)| s <= n && n <= e) {
            '>'
        } else {
            ' '
        };
        out.push_str(&format!("{mark}{n:>6}  {l}\n"));
    }
    (out, from > first || to < last)
}

/// `docmonk_read`: the verified content of `citation`, with `context`
/// lines (code) or chunks (documents) around it.
pub fn read_value(
    home: &Home,
    cfg: &DocMonkConfig,
    citation: &EvidenceRef,
    context: usize,
) -> Result<Value, DocMonkError> {
    let opened = open_sources(home, Some(&citation.source_id))?;
    let corp = corpora(&opened);
    let refs = [citation.clone()];
    let contents = grounding::canonical_contents(&corp, &refs).map_err(search_err)?;
    let check = grounding::verify_with(&corp, &refs, &SearchFilters::default(), &contents)
        .map_err(search_err)?
        .remove(0);
    let root = opened.first().map(|o| o.source.path.clone());
    let path = match &root {
        Some(r) if !citation.path.is_empty() => join_location(r, &citation.path),
        _ => citation.path.clone(),
    };
    let base = json!({
        "citation": citation.id,
        "source_id": citation.source_id,
        "build_id": citation.build_id,
        "kind": citation.kind,
        "record_id": citation.record_id,
        "path": path,
        "valid": check.valid,
        "reason": check.reason,
    });
    let Some(Some(k)) = contents.get(&citation.id).filter(|_| check.valid) else {
        let mut v = base;
        v["text"] = Value::Null;
        v["hint"] = json!(
            "the citation does not match the current index (see reason); run docmonk_evidence \
             again for a current citation"
        );
        return Ok(v);
    };
    let mut v = base;
    v["line_start"] = json!(citation.line_start);
    v["line_end"] = json!(citation.line_end);
    v["page_start"] = json!(citation.page_start);
    v["page_end"] = json!(citation.page_end);
    v["record_lines"] = json!(k.lines.map(|(s, e)| [s, e]));
    v["language"] = json!(k.language);
    match (citation.kind.as_str(), k.lines) {
        ("entity" | "passage", Some((first, _))) => {
            let cited = citation
                .line_start
                .map(|s| (s, citation.line_end.unwrap_or(s)));
            let context = context.min(MAX_CONTEXT_LINES);
            let (text, clipped) = numbered(&k.text, first, cited, context);
            v["text"] = json!(text);
            v["context_clipped"] = json!(clipped);
        }
        ("document", _) => {
            v["text"] = json!(k.text);
            let o = &opened[0];
            let opts = ContextOptions {
                parent_heading: true,
                previous_chunks: context.min(10) as i64,
                next_chunks: context.min(10) as i64,
                ..ContextOptions::from_config(&cfg.search.context)
            };
            v["context"] =
                expand_chunk_context(o.store.read(), &o.build, &citation.record_id, &opts)
                    .map_err(search_err)?
                    .unwrap_or(Value::Null);
        }
        _ => v["text"] = json!(k.text),
    }
    Ok(v)
}

/// A citation from tool arguments: the fields `evidence` returns for it.
pub fn citation_from(args: &serde_json::Map<String, Value>) -> Result<EvidenceRef, DocMonkError> {
    let s = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_owned);
    let n = |k: &str| args.get(k).and_then(Value::as_i64);
    let required = |k: &str| {
        s(k).filter(|v| !v.trim().is_empty())
            .ok_or_else(|| DocMonkError::usage(format!("{k} must not be empty")))
    };
    Ok(EvidenceRef {
        id: s("id").unwrap_or_else(|| "E1".into()),
        source_id: required("source_id")?,
        build_id: required("build_id")?,
        kind: required("kind")?,
        record_id: required("record_id")?,
        path: s("path").unwrap_or_default(),
        line_start: n("line_start"),
        line_end: n("line_end"),
        page_start: n("page_start"),
        page_end: n("page_end"),
        heading: s("heading"),
        fingerprint: s("fingerprint").unwrap_or_default(),
        content_fingerprint: s("content_fingerprint"),
        language: s("language"),
        text: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_is_numbered_with_the_cited_lines_marked() {
        let text = "a\nb\nc\nd\ne";
        let (out, clipped) = numbered(text, 10, Some((12, 12)), 1);
        assert_eq!(out, "     11  b\n>    12  c\n     13  d\n");
        assert!(clipped);
        let (all, clipped) = numbered(text, 10, None, 0);
        assert_eq!(all.lines().count(), 5);
        assert!(!clipped);
    }
}
