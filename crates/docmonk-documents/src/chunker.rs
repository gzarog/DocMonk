//! Payload-aware chunking.
//!
//! Invariant for every emitted chunk: the exact token count of its
//! contextual payload (with special tokens) is at most
//! `resolved_max_tokens - safety_tokens`, except a single table cell
//! fragment that cannot be cut further. Headings and tables stay their own
//! chunks; paragraph runs under one heading are packed with overlap and
//! peer rebalancing; oversized tables split at row, then cell, boundaries
//! with header rows repeated.

use std::collections::{BTreeMap, HashMap};

use docmonk_config::model::ChunkingConfig;
use serde::Serialize;

use crate::model::{Chunk, NormalizedDocument, NormalizedUnit, UnitKind};
use crate::table::{self, Row};
use crate::tokenization::{count_tokens, split_by_token_budget};
use crate::tokenizer::{model_tokenizer, preprocessing_fingerprint, ModelTokenizer};

/// Gates chunk boundaries and `search_text`. (3: sentences also end at
/// `。！？` and the Arabic and Devanagari full stops, and Chinese and
/// Japanese pieces are rejoined without inserted spaces.)
pub const CHUNKER_VERSION: &str = "3";
/// Gates `contextual_text` assembly.
pub const EMBEDDING_TEXT_VERSION: &str = "2";

/// `chunker_version` as stamped on files: version plus tokenizer identity.
pub fn chunker_version_stamp() -> String {
    format!("{CHUNKER_VERSION}+{}", preprocessing_fingerprint())
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ChunkingDiagnostics {
    pub chunks_split_by_budget: i64,
    pub context_headers_reduced: i64,
    pub oversized_table_rows: i64,
    pub oversized_table_cells: i64,
    pub max_payload_tokens: i64,
    pub reductions_by_kind: BTreeMap<String, i64>,
}

#[derive(Debug, Clone)]
struct Piece {
    text: String,
    page_start: Option<i64>,
    page_end: Option<i64>,
    tokens: i64,
}

#[derive(Debug, Clone, Copy)]
struct Budget {
    body_max: i64,
    overlap: i64,
    min_tokens: i64,
    merge_peers: bool,
}

fn header_text(doc_title: &str, segments: &[String]) -> String {
    let mut lines = Vec::new();
    if !doc_title.is_empty() {
        lines.push(format!("Document: {doc_title}"));
    }
    if !segments.is_empty() {
        lines.push(format!("Section: {}", segments.join(" > ")));
    }
    lines.join("\n")
}

fn contextual_text(header: &str, body: &str) -> String {
    if header.is_empty() {
        return body.to_owned();
    }
    if body.is_empty() {
        header.to_owned()
    } else {
        format!("{header}\n\n{body}")
    }
}

fn search_text(doc_title: &str, heading_path: &[String], body: &str) -> String {
    let mut lines: Vec<&str> = std::iter::once(doc_title)
        .chain(heading_path.iter().map(String::as_str))
        .filter(|l| !l.is_empty())
        .collect();
    if body.is_empty() {
        return lines.join("\n");
    }
    lines.push(body);
    lines.join("\n")
}

fn truncate_to_tokens(tok: &ModelTokenizer, text: &str, budget: i64) -> String {
    if budget <= 0 {
        return String::new();
    }
    if tok.count(text, false) <= budget {
        return text.to_owned();
    }
    tok.split(text, budget)
        .into_iter()
        .next()
        .unwrap_or_default()
}

fn fit_header(
    tok: &ModelTokenizer,
    doc_title: &str,
    heading_path: &[String],
    header_budget: i64,
) -> (String, bool) {
    let fits =
        |title: &str, segs: &[String]| tok.count(&header_text(title, segs), false) <= header_budget;
    let mut segments: &[String] = heading_path;
    if fits(doc_title, segments) {
        return (header_text(doc_title, segments), false);
    }
    if segments.len() > 1 {
        for drop in 1..segments.len() {
            if fits(doc_title, &segments[drop..]) {
                return (header_text(doc_title, &segments[drop..]), true);
            }
        }
        segments = &segments[segments.len() - 1..];
    }
    if !doc_title.is_empty() && fits("", segments) {
        return (header_text("", segments), true);
    }
    if let Some(deepest) = segments.first() {
        let prefix = tok.count("Section: ", false);
        let t = truncate_to_tokens(tok, deepest, (header_budget - prefix).max(1));
        return (header_text("", &[t]), true);
    }
    if !doc_title.is_empty() {
        let prefix = tok.count("Document: ", false);
        let t = truncate_to_tokens(tok, doc_title, (header_budget - prefix).max(1));
        return (header_text(&t, &[]), true);
    }
    (String::new(), false)
}

struct RunBudgeter<'a> {
    tok: &'a ModelTokenizer,
    cfg: &'a ChunkingConfig,
    available: i64,
    header_budget: i64,
}

impl<'a> RunBudgeter<'a> {
    fn new(tok: &'a ModelTokenizer, cfg: &'a ChunkingConfig) -> Self {
        let payload_budget = cfg.resolved_max_tokens() - cfg.safety_tokens;
        let available = payload_budget - tok.special_tokens();
        let min_body = cfg.min_tokens.min(available - 1).max(1);
        Self {
            tok,
            cfg,
            available,
            header_budget: (available - min_body).max(1),
        }
    }

    fn fit_header(&self, doc_title: &str, heading_path: &[String]) -> (String, bool) {
        fit_header(self.tok, doc_title, heading_path, self.header_budget)
    }

    fn body_budget(&self, header: &str) -> Budget {
        let body_max = (self.available - self.tok.count(header, false)).max(1);
        Budget {
            body_max,
            overlap: if body_max > 1 {
                self.cfg.overlap_tokens.min(body_max - 1)
            } else {
                0
            },
            min_tokens: self.cfg.min_tokens.min(body_max),
            merge_peers: self.cfg.merge_peers,
        }
    }
}

fn pack_pieces(pieces: &[Piece], budget: Budget) -> Vec<Vec<Piece>> {
    let mut groups: Vec<Vec<Piece>> = Vec::new();
    let mut overlap_seed: Vec<Piece> = Vec::new();
    let n = pieces.len();
    let mut i = 0;
    while i < n {
        let mut current: Vec<Piece> = Vec::new();
        let mut current_tokens = 0;
        if !overlap_seed.is_empty() && budget.overlap > 0 {
            let next_tokens = pieces[i].tokens;
            let mut seed: Vec<Piece> = Vec::new();
            let mut seed_tokens = 0;
            for p in overlap_seed.iter().rev() {
                if seed_tokens + p.tokens > budget.overlap
                    || seed_tokens + p.tokens + next_tokens > budget.body_max
                {
                    break;
                }
                seed.insert(0, p.clone());
                seed_tokens += p.tokens;
            }
            current = seed;
            current_tokens = seed_tokens;
        }
        let mut started_new = false;
        while i < n {
            let p = &pieces[i];
            if !current.is_empty() && started_new && current_tokens + p.tokens > budget.body_max {
                break;
            }
            current.push(p.clone());
            current_tokens += p.tokens;
            started_new = true;
            i += 1;
            if current_tokens >= budget.body_max {
                break;
            }
        }
        overlap_seed = current.clone();
        groups.push(current);
    }
    groups
}

fn group_tokens(g: &[Piece]) -> i64 {
    g.iter().map(|p| p.tokens).sum()
}

fn split_evenly(pieces: Vec<Piece>) -> (Vec<Piece>, Vec<Piece>) {
    let total = group_tokens(&pieces);
    let target = (total + 1).div_euclid(2);
    let mut first_tokens = 0;
    let mut split_at = pieces.len();
    for (idx, p) in pieces.iter().enumerate() {
        if idx > 0 && first_tokens + p.tokens > target {
            split_at = idx;
            break;
        }
        first_tokens += p.tokens;
    }
    let mut first = pieces;
    let second = first.split_off(split_at);
    (first, second)
}

fn merge_peers(groups: Vec<Vec<Piece>>, budget: Budget) -> Vec<Vec<Piece>> {
    if !budget.merge_peers || groups.len() < 2 {
        return groups;
    }
    let mut result = groups;
    for i in 0..result.len() - 1 {
        if group_tokens(&result[i]) >= budget.min_tokens
            && group_tokens(&result[i + 1]) >= budget.min_tokens
        {
            continue;
        }
        let mut both = result[i].clone();
        both.extend(result[i + 1].iter().cloned());
        let (first, second) = split_evenly(both);
        if first.is_empty() || second.is_empty() {
            continue;
        }
        if group_tokens(&first) <= budget.body_max && group_tokens(&second) <= budget.body_max {
            result[i] = first;
            result[i + 1] = second;
        }
    }
    result
}

struct Ctx<'a> {
    budgeter: RunBudgeter<'a>,
    doc_title: &'a str,
    diagnostics: Option<&'a mut ChunkingDiagnostics>,
}

impl Ctx<'_> {
    fn make_contextual(&mut self, heading_path: &[String], body: &str, kind: &str) -> String {
        let (header, reduced) = self.budgeter.fit_header(self.doc_title, heading_path);
        if reduced {
            if let Some(d) = self.diagnostics.as_deref_mut() {
                d.context_headers_reduced += 1;
                *d.reductions_by_kind.entry(kind.to_owned()).or_insert(0) += 1;
            }
        }
        contextual_text(&header, body)
    }

    fn table_chunk(
        &mut self,
        unit: &NormalizedUnit,
        rows: Vec<Row>,
        parent_index: Option<usize>,
        caption: Option<String>,
    ) -> Chunk {
        let caption = caption.or_else(|| unit.caption.clone());
        let rendered = table::render_table(&rows, caption.as_deref());
        Chunk {
            kind: UnitKind::Table,
            text: String::new(),
            heading_level: None,
            heading_path: unit.heading_path.clone(),
            parent_index,
            page_start: unit.page_start,
            page_end: unit.page_end,
            table_rows: Some(rows),
            caption,
            contextual_text: self.make_contextual(&unit.heading_path, &rendered, "table"),
            search_text: search_text(self.doc_title, &unit.heading_path, &rendered),
            token_count: count_tokens(&rendered),
        }
    }

    fn table_chunks(&mut self, unit: &NormalizedUnit, parent_index: Option<usize>) -> Vec<Chunk> {
        let rows = unit.table_rows.clone().unwrap_or_default();
        let (header, _) = self.budgeter.fit_header(self.doc_title, &unit.heading_path);
        let budget = self.budgeter.body_budget(&header);
        let whole = self.table_chunk(unit, rows.clone(), parent_index, None);
        if whole.token_count <= budget.body_max || rows.is_empty() {
            return vec![whole];
        }
        let hc = unit.header_row_count.min(rows.len());
        let (header_rows, data_rows) = rows.split_at(hc);
        let base_caption = unit.caption.clone().unwrap_or_default();
        let groups = table::split_data_rows(data_rows, header_rows, budget.body_max, &base_caption);
        if groups.is_empty() {
            return vec![whole];
        }
        let mut result = Vec::new();
        let mut offset = 0;
        for group in groups {
            let mut seg_rows = header_rows.to_vec();
            seg_rows.extend(group.iter().cloned());
            let chunk = self.table_chunk(unit, seg_rows, parent_index, None);
            if chunk.token_count <= budget.body_max {
                result.push(chunk);
                offset += group.len();
                continue;
            }
            let row = group[0].clone();
            let row_number = offset + 1;
            offset += group.len();
            let seg_caption = if base_caption.is_empty() {
                format!("Row {row_number}")
            } else {
                format!("{base_caption}\nRow {row_number}")
            };
            if let Some(d) = self.diagnostics.as_deref_mut() {
                d.oversized_table_rows += 1;
                for (col, cell) in row.iter().enumerate() {
                    let h: Row = header_rows
                        .iter()
                        .map(|hr| hr.get(col).cloned().unwrap_or_default())
                        .collect();
                    let single = table::render_table(&[h, vec![cell.clone()]], Some(&seg_caption));
                    if count_tokens(&single) > budget.body_max {
                        d.oversized_table_cells += 1;
                    }
                }
            }
            for (hs, ds) in
                table::segment_oversized_row(&row, header_rows, budget.body_max, &seg_caption)
            {
                let mut seg = hs;
                seg.push(ds);
                result.push(self.table_chunk(unit, seg, parent_index, Some(seg_caption.clone())));
            }
        }
        result
    }
}

/// Chunks `doc` under exact, payload-aware token budgets.
pub fn chunk_document(
    doc: &NormalizedDocument,
    cfg: &ChunkingConfig,
    doc_title: &str,
    diagnostics: Option<&mut ChunkingDiagnostics>,
) -> Vec<Chunk> {
    let tok = model_tokenizer().expect("bundled tokenizer");
    let mut ctx = Ctx {
        budgeter: RunBudgeter::new(tok, cfg),
        doc_title,
        diagnostics,
    };
    let mut heading_map: HashMap<usize, usize> = HashMap::new();
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut pending: Vec<&NormalizedUnit> = Vec::new();

    fn flush(
        ctx: &mut Ctx<'_>,
        pending: &mut Vec<&NormalizedUnit>,
        chunks: &mut Vec<Chunk>,
        heading_map: &HashMap<usize, usize>,
    ) {
        let Some(first) = pending.first().copied() else {
            return;
        };
        let parent = first
            .parent_index
            .and_then(|p| heading_map.get(&p).copied());
        let (header, _) = ctx.budgeter.fit_header(ctx.doc_title, &first.heading_path);
        let budget = ctx.budgeter.body_budget(&header);
        let mut pieces = Vec::new();
        for unit in pending.iter() {
            let parts = split_by_token_budget(&unit.text, budget.body_max);
            if parts.len() > 1 {
                if let Some(d) = ctx.diagnostics.as_deref_mut() {
                    d.chunks_split_by_budget += 1;
                }
            }
            pieces.extend(parts.into_iter().map(|t| Piece {
                tokens: count_tokens(&t),
                text: t,
                page_start: unit.page_start,
                page_end: unit.page_end,
            }));
        }
        for group in merge_peers(pack_pieces(&pieces, budget), budget) {
            if group.is_empty() {
                continue;
            }
            let text = group
                .iter()
                .map(|p| p.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            let pages: Vec<i64> = group
                .iter()
                .flat_map(|p| [p.page_start, p.page_end])
                .flatten()
                .collect();
            let contextual = ctx.make_contextual(&first.heading_path, &text, "paragraph");
            chunks.push(Chunk {
                kind: UnitKind::Paragraph,
                heading_level: None,
                heading_path: first.heading_path.clone(),
                parent_index: parent,
                page_start: pages.iter().min().copied(),
                page_end: pages.iter().max().copied(),
                table_rows: None,
                caption: None,
                contextual_text: contextual,
                search_text: search_text(ctx.doc_title, &first.heading_path, &text),
                token_count: count_tokens(&text),
                text,
            });
        }
        pending.clear();
    }

    for (old_index, unit) in doc.units.iter().enumerate() {
        if unit.kind == UnitKind::Paragraph {
            pending.push(unit);
            continue;
        }
        flush(&mut ctx, &mut pending, &mut chunks, &heading_map);
        let parent = unit.parent_index.and_then(|p| heading_map.get(&p).copied());
        if unit.kind == UnitKind::Table {
            let t = ctx.table_chunks(unit, parent);
            chunks.extend(t);
            continue;
        }
        let countable = unit.text.clone();
        let new_index = chunks.len();
        let contextual = ctx.make_contextual(&unit.heading_path, &countable, unit.kind.as_str());
        chunks.push(Chunk {
            kind: unit.kind,
            text: unit.text.clone(),
            heading_level: unit.heading_level,
            heading_path: unit.heading_path.clone(),
            parent_index: parent,
            page_start: unit.page_start,
            page_end: unit.page_end,
            table_rows: unit.table_rows.clone(),
            caption: unit.caption.clone(),
            contextual_text: contextual,
            search_text: search_text(doc_title, &unit.heading_path, &countable),
            token_count: count_tokens(&countable),
        });
        heading_map.insert(old_index, new_index);
    }
    flush(&mut ctx, &mut pending, &mut chunks, &heading_map);
    if let Some(d) = ctx.diagnostics.as_deref_mut() {
        if let Some(max) = chunks
            .iter()
            .map(|c| tok.count(&c.contextual_text, true))
            .max()
        {
            d.max_payload_tokens = d.max_payload_tokens.max(max);
        }
    }
    chunks
}
