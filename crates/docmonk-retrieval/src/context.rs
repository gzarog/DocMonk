//! Search context expansion (`retrieval/context_builder.expand_chunk_context`):
//! a matched document chunk with its enclosing heading and nearest
//! siblings under that heading. This runs only on hits already ranked and
//! cut to the limit. It changes what is shown, never what was selected.

use docmonk_storage::knowledge::ChunkRow;
use docmonk_storage::read::KnowledgeRead;
use serde_json::{json, Value};

use crate::SearchError;

/// `search.context`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextOptions {
    pub parent_heading: bool,
    pub previous_chunks: i64,
    pub next_chunks: i64,
    pub max_tokens: i64,
}

impl ContextOptions {
    pub fn from_config(c: &docmonk_config::model::SearchContextConfig) -> Self {
        Self {
            parent_heading: c.parent_heading,
            previous_chunks: c.previous_chunks,
            next_chunks: c.next_chunks,
            max_tokens: c.max_tokens,
        }
    }

    /// Fully off: nothing to look up.
    pub fn disabled(&self) -> bool {
        !self.parent_heading && self.previous_chunks == 0 && self.next_chunks == 0
    }
}

/// The chunk's text as presented: a table's is its row-aware
/// rendering (the store keeps the rows, not the rendering).
pub fn text_of(c: &ChunkRow) -> String {
    match (&c.table_rows, c.kind.as_str()) {
        (Some(rows), "table") if c.text.is_empty() => {
            docmonk_documents::table::render_table(rows, c.caption.as_deref())
        }
        _ => c.text.clone(),
    }
}

fn piece(c: &ChunkRow) -> Value {
    json!({
        "id": c.id,
        "kind": c.kind,
        "text": text_of(c),
        "heading_path": c.heading_path,
        "page_start": c.page_start,
        "page_end": c.page_end,
    })
}

fn tokens(text: &str) -> i64 {
    if text.is_empty() {
        return 0;
    }
    docmonk_documents::tokenizer::model_tokenizer()
        .map(|t| t.count(text, false))
        // The tokenizer is bundled and verified; a failure here would
        // already have failed indexing. Fall back to a whitespace count.
        .unwrap_or_else(|_| text.split_whitespace().count() as i64)
}

/// `None` when `chunk_id` is not a stored chunk (an entity or path hit).
/// The matched chunk always counts against `max_tokens` but is never
/// dropped. The heading is spent first, then siblings nearest-first per
/// side. The first piece that overflows ends that side, so the window
/// stays contiguous.
pub fn expand_chunk_context(
    store: &dyn KnowledgeRead,
    build_id: &str,
    chunk_id: &str,
    opts: &ContextOptions,
) -> Result<Option<Value>, SearchError> {
    let Some(matched) = store.chunk(build_id, chunk_id)? else {
        return Ok(None);
    };
    let budget = opts.max_tokens;
    let mut used = tokens(&text_of(&matched));
    let mut reasons: Vec<String> = Vec::new();

    let mut parent = None;
    if opts.parent_heading {
        if let Some(p) = matched.parent_ordinal {
            if let Some(h) = store.chunk_at(build_id, &matched.document_id, p)? {
                let cost = tokens(&text_of(&h));
                if used + cost <= budget {
                    used += cost;
                    parent = Some(piece(&h));
                } else {
                    reasons.push(format!(
                        "parent heading dropped: max_tokens={budget} reached"
                    ));
                }
            }
        }
    }
    let (before, after) =
        store.chunk_siblings(build_id, &matched, opts.previous_chunks, opts.next_chunks)?;
    let mut take = |nearest_first: Vec<&ChunkRow>| -> Vec<Value> {
        let mut taken = Vec::new();
        for c in nearest_first {
            let cost = tokens(&text_of(c));
            if used + cost > budget {
                reasons.push(format!(
                    "sibling chunk dropped: max_tokens={budget} reached"
                ));
                break;
            }
            used += cost;
            taken.push(piece(c));
        }
        taken
    };
    let mut previous = take(before.iter().rev().collect());
    previous.reverse();
    let next = take(after.iter().collect());
    Ok(Some(json!({
        "matched": piece(&matched),
        "parent_heading": parent,
        "previous": previous,
        "next": next,
        "truncated": !reasons.is_empty(),
        "truncation_reasons": reasons,
    })))
}
