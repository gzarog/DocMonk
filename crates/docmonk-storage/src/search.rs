//! Lexical search projections over one build. Each query returns
//! rows with their file already joined in (entity, document and
//! file search projections).
//! Ranking and tiering live in `docmonk-retrieval`.

use crate::error::{Result, StorageError};
use crate::knowledge::{
    entity_from_row, relationship_from_row, EntityRow, ProjectStore, RelationshipRow,
    ENTITY_COLUMNS, RELATIONSHIP_COLUMNS,
};

/// Document-FTS column weights (`bm25(chunk_fts, ...)`): chunk id and build
/// id are unindexed placeholders, then heading 5, body 1, title 8 and the
/// identifier parts 0.5.
const CHUNK_BM25: &str = "bm25(chunk_fts, 0.0, 0.0, 5.0, 1.0, 8.0, 0.5)";
/// Code-FTS weights: name 10, qualified name 5, signature 2, and the
/// name's parts 10 (`project store` finds `ProjectStore` as readily as
/// `project_store`; shorter names rank first).
const CODE_BM25: &str = "bm25(code_fts, 0.0, 0.0, 10.0, 5.0, 2.0, 10.0)";
/// Path-FTS weights: path 2, identifier parts 1.
const PATH_BM25: &str = "bm25(path_fts, 0.0, 0.0, 2.0, 1.0)";
/// Passage-FTS weights: owner (qualified name and signature) 3, body 1,
/// identifier parts 0.5.
const PASSAGE_BM25: &str = "bm25(passage_fts, 0.0, 0.0, 3.0, 1.0, 0.5)";

/// An implementation passage hit with its file and owning entity joined in.
#[derive(Debug, Clone, PartialEq)]
pub struct PassageSearchRow {
    pub id: String,
    pub file_id: String,
    pub kind: String,
    pub extraction: String,
    pub language: Option<String>,
    pub start_line: i64,
    pub end_line: i64,
    pub header: String,
    pub rel_path: String,
    pub mtime: f64,
    /// A match-centred excerpt of the passage text.
    pub snippet: Option<String>,
    /// The owning entity of a `body` passage.
    pub entity: Option<PassageOwner>,
    pub fts_rank: u32,
    pub bm25: Option<f64>,
}

/// The entity a body passage belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassageOwner {
    pub id: String,
    pub name: String,
    pub qualified_name: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EntitySearchRow {
    pub id: String,
    pub name: String,
    pub qualified_name: String,
    pub kind: String,
    pub signature: Option<String>,
    pub start_line: i64,
    pub end_line: i64,
    pub rel_path: String,
    pub mtime: f64,
    /// Position in the FTS result (0 for non-FTS lookups).
    pub fts_rank: u32,
    pub bm25: Option<f64>,
}

/// Provenance of a hit inside an email attachment.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AttachmentProvenance {
    pub name: Option<String>,
    pub content_type: Option<String>,
    pub index: Option<i64>,
    pub format: String,
    pub parent_document_id: String,
    pub parent_title: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DocumentSearchRow {
    /// Chunk id for FTS hits, document id for title hits.
    pub id: String,
    /// The document the hit belongs to.
    pub document_id: String,
    pub title: String,
    pub rel_path: String,
    pub mtime: f64,
    pub snippet: Option<String>,
    pub heading: Option<String>,
    pub heading_path: Vec<String>,
    pub page_start: Option<i64>,
    pub page_end: Option<i64>,
    pub attachment: Option<AttachmentProvenance>,
    pub fts_rank: u32,
    pub bm25: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PathSearchRow {
    pub id: String,
    pub rel_path: String,
    pub mtime: f64,
}

const ENTITY_PROJECTION: &str = "SELECT e.id, e.name, e.qualified_name, e.kind, e.signature,
        e.start_line, e.end_line, f.rel_path, f.mtime
     FROM entities e JOIN files f ON f.build_id = e.build_id AND f.id = e.file_id";

fn entity_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<EntitySearchRow> {
    Ok(EntitySearchRow {
        id: r.get(0)?,
        name: r.get(1)?,
        qualified_name: r.get(2)?,
        kind: r.get(3)?,
        signature: r.get(4)?,
        start_line: r.get(5)?,
        end_line: r.get(6)?,
        rel_path: r.get(7)?,
        mtime: r.get(8)?,
        fts_rank: 0,
        bm25: None,
    })
}

const ATTACHMENT_COLUMNS: &str = "d.parent_document_id, d.attachment_name,
    d.attachment_content_type, d.attachment_index, d.format, p.title";

/// Reads the six [`ATTACHMENT_COLUMNS`] starting at `at`.
fn attachment(r: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<Option<AttachmentProvenance>> {
    let parent: Option<String> = r.get(at)?;
    Ok(match parent {
        None => None,
        Some(parent_document_id) => Some(AttachmentProvenance {
            name: r.get(at + 1)?,
            content_type: r.get(at + 2)?,
            index: r.get(at + 3)?,
            format: r.get(at + 4)?,
            parent_document_id,
            parent_title: r.get(at + 5)?,
        }),
    })
}

pub use crate::read::ReadFilter;

/// `AND (col LIKE ?n ESCAPE '\' OR ...)` over `count` patterns numbered
/// from `first`; empty without patterns.
fn path_clause(column: &str, count: usize, first: usize) -> String {
    if count == 0 {
        return String::new();
    }
    let ors = (0..count)
        .map(|i| format!("{column} LIKE ?{} ESCAPE '\\'", first + i))
        .collect::<Vec<_>>()
        .join(" OR ");
    format!("AND ({ors})")
}

impl ReadFilter {
    /// `LIKE` patterns of the path prefixes.
    fn like_patterns(&self) -> Vec<String> {
        self.path_prefixes
            .iter()
            .map(|p| format!("{}%", like_literal(p)))
            .collect()
    }
}

/// SQL `LIKE` pattern matching `s` literally (escape `\`).
fn like_literal(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

impl ProjectStore {
    /// Entities whose name or qualified name equals `q`.
    pub fn search_entities_exact(&self, build_id: &str, q: &str) -> Result<Vec<EntitySearchRow>> {
        self.query_rows(
            "entity exact search",
            &format!(
                "{ENTITY_PROJECTION} WHERE e.build_id = ?1 AND (e.name = ?2 OR e.qualified_name = ?2)
                 ORDER BY e.qualified_name, e.file_id, e.start_line"
            ),
            &[&build_id, &q],
            entity_row,
        )
    }

    /// Entities whose alias equals `alias`. The alias is the last two
    /// dot-separated segments of a qualified name with at least three
    /// segments, so only a query with
    /// exactly one dot can match.
    pub fn search_entities_alias(
        &self,
        build_id: &str,
        alias: &str,
        limit: i64,
    ) -> Result<Vec<EntitySearchRow>> {
        if alias.matches('.').count() != 1 {
            return Ok(Vec::new());
        }
        let pattern = format!("%.{}", like_literal(alias));
        self.query_rows(
            "entity alias search",
            &format!(
                "{ENTITY_PROJECTION} WHERE e.build_id = ?1
                   AND e.qualified_name LIKE ?2 ESCAPE '\\'
                   AND length(e.qualified_name) - length(replace(e.qualified_name, '.', '')) >= 2
                 ORDER BY e.qualified_name, e.file_id, e.start_line LIMIT ?3"
            ),
            &[&build_id, &pattern, &limit],
            entity_row,
        )
    }

    /// BM25-ranked entities for an FTS5 `expression` (already sanitized).
    /// Equal scores are ordered by path, qualified name and line, never by
    /// insertion order (which depends on worker scheduling).
    pub fn search_entities_fts(
        &self,
        build_id: &str,
        expression: &str,
        limit: i64,
    ) -> Result<Vec<EntitySearchRow>> {
        self.search_entities_fts_in(build_id, expression, limit, &ReadFilter::default())
    }

    /// [`Self::search_entities_fts`] restricted to `filter`'s paths in SQL,
    /// so `limit` counts only in-scope rows.
    pub fn search_entities_fts_in(
        &self,
        build_id: &str,
        expression: &str,
        limit: i64,
        filter: &ReadFilter,
    ) -> Result<Vec<EntitySearchRow>> {
        if !filter.document_ids.is_empty() {
            // A document filter admits no entity.
            return Ok(Vec::new());
        }
        let patterns = filter.like_patterns();
        let clause = path_clause("f.rel_path", patterns.len(), 4);
        let mut args: Vec<&dyn rusqlite::ToSql> = vec![&expression, &build_id, &limit];
        args.extend(patterns.iter().map(|p| p as &dyn rusqlite::ToSql));
        let mut rows = self.query_rows(
            "entity fts search",
            &format!(
                "SELECT e.id, e.name, e.qualified_name, e.kind, e.signature,
                    e.start_line, e.end_line, f.rel_path, f.mtime, {CODE_BM25}
                 FROM code_fts
                 JOIN entities e ON e.rowid = code_fts.rowid
                 JOIN files f ON f.build_id = e.build_id AND f.id = e.file_id
                 WHERE code_fts MATCH ?1 AND code_fts.build_id = ?2 {clause}
                 ORDER BY {CODE_BM25}, f.rel_path, e.qualified_name, e.start_line, e.id
                 LIMIT ?3"
            ),
            &args,
            |r| {
                let mut row = entity_row(r)?;
                row.bm25 = Some(r.get(9)?);
                Ok(row)
            },
        )?;
        for (i, row) in rows.iter_mut().enumerate() {
            row.fts_rank = i as u32;
        }
        Ok(rows)
    }

    /// Documents whose title equals `title`, case-insensitively.
    pub fn search_document_titles(
        &self,
        build_id: &str,
        title: &str,
        limit: i64,
    ) -> Result<Vec<DocumentSearchRow>> {
        self.query_rows(
            "document title search",
            &format!(
                "SELECT d.id, d.title, f.rel_path, f.mtime, {ATTACHMENT_COLUMNS}
                 FROM documents d
                 JOIN files f ON f.build_id = d.build_id AND f.id = d.file_id
                 LEFT JOIN documents p ON p.build_id = d.build_id AND p.id = d.parent_document_id
                 WHERE d.build_id = ?1 AND d.title = ?2 COLLATE NOCASE
                 LIMIT ?3"
            ),
            &[&build_id, &title, &limit],
            |r| {
                let title: String = r.get(1)?;
                Ok(DocumentSearchRow {
                    id: r.get(0)?,
                    document_id: r.get(0)?,
                    snippet: Some(title.clone()),
                    title,
                    rel_path: r.get(2)?,
                    mtime: r.get(3)?,
                    heading: None,
                    heading_path: Vec::new(),
                    page_start: None,
                    page_end: None,
                    attachment: attachment(r, 4)?,
                    fts_rank: 0,
                    bm25: None,
                })
            },
        )
    }

    /// BM25-ranked chunks (heading 5, body 1, title 8) for an FTS5
    /// `expression` (equal scores by path and chunk order). Each carries a
    /// match-centred snippet of at most
    /// `snippet_tokens` tokens (clamped to 1..=64).
    pub fn search_chunks_fts(
        &self,
        build_id: &str,
        expression: &str,
        limit: i64,
        snippet_tokens: i64,
    ) -> Result<Vec<DocumentSearchRow>> {
        self.search_chunks_fts_in(
            build_id,
            expression,
            limit,
            snippet_tokens,
            &ReadFilter::default(),
        )
    }

    /// [`Self::search_chunks_fts`] restricted to `filter` (paths and
    /// documents) in SQL, so `limit` counts only in-scope rows.
    pub fn search_chunks_fts_in(
        &self,
        build_id: &str,
        expression: &str,
        limit: i64,
        snippet_tokens: i64,
        filter: &ReadFilter,
    ) -> Result<Vec<DocumentSearchRow>> {
        let tokens = snippet_tokens.clamp(1, 64);
        let patterns = filter.like_patterns();
        let clause = path_clause("f.rel_path", patterns.len(), 5);
        let first_doc = 5 + patterns.len();
        let docs = if filter.document_ids.is_empty() {
            String::new()
        } else {
            let marks = (0..filter.document_ids.len())
                .map(|i| format!("?{}", first_doc + i))
                .collect::<Vec<_>>()
                .join(",");
            format!("AND c.document_id IN ({marks})")
        };
        let mut args: Vec<&dyn rusqlite::ToSql> = vec![&tokens, &expression, &build_id, &limit];
        args.extend(patterns.iter().map(|p| p as &dyn rusqlite::ToSql));
        args.extend(
            filter
                .document_ids
                .iter()
                .map(|d| d as &dyn rusqlite::ToSql),
        );
        let mut stmt = self
            .conn
            .prepare_cached(&format!(
                "SELECT c.id, fts.heading, fts.body, fts.title, d.title, f.rel_path, f.mtime,
                        c.page_start, c.page_end, c.heading_path, {ATTACHMENT_COLUMNS},
                        {CHUNK_BM25}, snippet(chunk_fts, -1, '', '', '...', ?1), c.document_id
                 FROM chunk_fts fts
                 JOIN chunks c ON c.rowid = fts.rowid
                 JOIN documents d ON d.build_id = c.build_id AND d.id = c.document_id
                 JOIN files f ON f.build_id = c.build_id AND f.id = c.file_id
                 LEFT JOIN documents p ON p.build_id = d.build_id AND p.id = d.parent_document_id
                 WHERE chunk_fts MATCH ?2 AND fts.build_id = ?3 {clause} {docs}
                 ORDER BY {CHUNK_BM25}, f.rel_path, c.ordinal, c.id LIMIT ?4"
            ))
            .map_err(StorageError::sqlite("chunk fts search"))?;
        let rows = stmt
            .query_map(args.as_slice(), |r| {
                let heading: String = r.get(1)?;
                let body: String = r.get(2)?;
                let fts_title: String = r.get(3)?;
                let doc_title: Option<String> = r.get(4)?;
                let rel_path: String = r.get(5)?;
                let heading_path: String = r.get(9)?;
                let snippet: Option<String> = r.get(17)?;
                let snippet = snippet
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty());
                let fallback: String = if body.is_empty() { &heading } else { &body }
                    .chars()
                    .take(crate::vectors::SNIPPET_CHARS)
                    .collect();
                let title = [Some(fts_title), doc_title]
                    .into_iter()
                    .flatten()
                    .find(|t| !t.is_empty())
                    .unwrap_or_else(|| rel_path.clone());
                Ok(DocumentSearchRow {
                    id: r.get(0)?,
                    document_id: r.get(18)?,
                    title,
                    rel_path,
                    mtime: r.get(6)?,
                    snippet: snippet.or((!fallback.is_empty()).then_some(fallback)),
                    heading: (!heading.is_empty()).then_some(heading),
                    heading_path: serde_json::from_str(&heading_path).unwrap_or_default(),
                    page_start: r.get(7)?,
                    page_end: r.get(8)?,
                    attachment: attachment(r, 10)?,
                    fts_rank: 0,
                    bm25: Some(r.get(16)?),
                })
            })
            .map_err(StorageError::sqlite("chunk fts search"))?;
        let mut out: Vec<DocumentSearchRow> = rows
            .collect::<rusqlite::Result<_>>()
            .map_err(StorageError::sqlite("chunk fts search"))?;
        for (i, row) in out.iter_mut().enumerate() {
            row.fts_rank = i as u32;
        }
        Ok(out)
    }

    /// BM25-ranked implementation passages for an FTS5 `expression`,
    /// restricted to `filter`'s paths in SQL (a document filter admits
    /// none). Equal scores are ordered by path and file order; each hit
    /// carries a match-centred snippet of at most `snippet_tokens` tokens.
    pub fn search_passages_fts_in(
        &self,
        build_id: &str,
        expression: &str,
        limit: i64,
        snippet_tokens: i64,
        filter: &ReadFilter,
    ) -> Result<Vec<PassageSearchRow>> {
        if !filter.document_ids.is_empty() {
            return Ok(Vec::new());
        }
        let tokens = snippet_tokens.clamp(1, 64);
        let patterns = filter.like_patterns();
        let clause = path_clause("f.rel_path", patterns.len(), 5);
        let mut args: Vec<&dyn rusqlite::ToSql> = vec![&tokens, &expression, &build_id, &limit];
        args.extend(patterns.iter().map(|p| p as &dyn rusqlite::ToSql));
        let mut rows = self.query_rows(
            "passage fts search",
            &format!(
                "SELECT p.id, p.file_id, p.kind, p.extraction, p.language, p.start_line,
                    p.end_line, p.header, f.rel_path, f.mtime,
                    snippet(passage_fts, 3, '', '', '...', ?1), {PASSAGE_BM25},
                    e.id, e.name, e.qualified_name, e.kind
                 FROM passage_fts
                 JOIN code_passages p ON p.rowid = passage_fts.rowid
                 JOIN files f ON f.build_id = p.build_id AND f.id = p.file_id
                 LEFT JOIN entities e ON e.build_id = p.build_id AND e.id = p.entity_id
                 WHERE passage_fts MATCH ?2 AND passage_fts.build_id = ?3 {clause}
                 ORDER BY {PASSAGE_BM25}, f.rel_path, p.ordinal, p.id
                 LIMIT ?4"
            ),
            &args,
            |r| {
                let snippet: Option<String> = r.get(10)?;
                let owner: Option<String> = r.get(12)?;
                Ok(PassageSearchRow {
                    id: r.get(0)?,
                    file_id: r.get(1)?,
                    kind: r.get(2)?,
                    extraction: r.get(3)?,
                    language: r.get(4)?,
                    start_line: r.get(5)?,
                    end_line: r.get(6)?,
                    header: r.get(7)?,
                    rel_path: r.get(8)?,
                    mtime: r.get(9)?,
                    snippet: snippet
                        .map(|s| s.trim().to_owned())
                        .filter(|s| !s.is_empty()),
                    bm25: Some(r.get(11)?),
                    entity: match owner {
                        Some(id) => Some(PassageOwner {
                            id,
                            name: r.get(13)?,
                            qualified_name: r.get(14)?,
                            kind: r.get(15)?,
                        }),
                        None => None,
                    },
                    fts_rank: 0,
                })
            },
        )?;
        for (i, row) in rows.iter_mut().enumerate() {
            row.fts_rank = i as u32;
        }
        Ok(rows)
    }

    /// Files whose path matches every token of `q` (FTS, BM25 then path
    /// order), falling back to a substring scan when that finds nothing
    /// (e.g. a mid-word fragment).
    pub fn search_paths_projection(
        &self,
        build_id: &str,
        q: &str,
        limit: i64,
    ) -> Result<Vec<PathSearchRow>> {
        self.search_paths_projection_in(build_id, q, limit, &ReadFilter::default())
    }

    /// [`Self::search_paths_projection`] restricted to `filter`'s paths in
    /// SQL.
    pub fn search_paths_projection_in(
        &self,
        build_id: &str,
        q: &str,
        limit: i64,
        filter: &ReadFilter,
    ) -> Result<Vec<PathSearchRow>> {
        if !filter.document_ids.is_empty() {
            return Ok(Vec::new());
        }
        let patterns = filter.like_patterns();
        let map = |r: &rusqlite::Row<'_>| -> rusqlite::Result<PathSearchRow> {
            Ok(PathSearchRow {
                id: r.get(0)?,
                rel_path: r.get(1)?,
                mtime: r.get(2)?,
            })
        };
        let tokens = word_tokens(q);
        if !tokens.is_empty() {
            let expression = tokens
                .iter()
                .map(|t| format!("\"{t}\""))
                .collect::<Vec<_>>()
                .join(" AND ");
            let clause = path_clause("f.rel_path", patterns.len(), 4);
            let mut args: Vec<&dyn rusqlite::ToSql> = vec![&expression, &build_id, &limit];
            args.extend(patterns.iter().map(|p| p as &dyn rusqlite::ToSql));
            let rows = self.query_rows(
                "path fts search",
                &format!(
                    "SELECT f.id, f.rel_path, f.mtime FROM path_fts
                     JOIN files f ON f.rowid = path_fts.rowid
                     WHERE path_fts MATCH ?1 AND path_fts.build_id = ?2 {clause}
                     ORDER BY {PATH_BM25}, f.rel_path LIMIT ?3"
                ),
                &args,
                map,
            )?;
            if !rows.is_empty() {
                return Ok(rows);
            }
        }
        let pattern = format!("%{}%", like_literal(q));
        let clause = path_clause("rel_path", patterns.len(), 4);
        let mut args: Vec<&dyn rusqlite::ToSql> = vec![&build_id, &pattern, &limit];
        args.extend(patterns.iter().map(|p| p as &dyn rusqlite::ToSql));
        self.query_rows(
            "path substring search",
            &format!(
                "SELECT id, rel_path, mtime FROM files
                 WHERE build_id = ?1 AND rel_path LIKE ?2 ESCAPE '\\' {clause}
                 ORDER BY rel_path LIMIT ?3"
            ),
            &args,
            map,
        )
    }
}

/// Word tokens (`\w+`, Unicode-aware).
pub fn word_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() || c == '_' || is_mark(c) {
            cur.push(c);
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The words inside identifiers of `text` that the FTS tokenizer does not
/// already separate, lowercased: every `_`-separated piece that splits on
/// a case change (camelCase, PascalCase, `HTTPServer`) or a letter/digit
/// boundary contributes its parts (`ProjectStore` -> `project store`,
/// `v2Api` -> `v 2 api`). `embedding_digest` contributes nothing: the
/// tokenizer splits on `_` itself. This is the `words` column of the FTS
/// tables, so natural-language queries find identifiers.
pub fn identifier_words(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for token in word_tokens(text) {
        for piece in token.split('_').filter(|p| !p.is_empty()) {
            let parts = split_identifier(piece);
            if parts.len() > 1 {
                out.extend(parts);
            }
        }
    }
    out.join(" ")
}

/// Parts of one identifier token, lowercased (see [`identifier_words`]).
pub fn split_identifier(token: &str) -> Vec<String> {
    let chars: Vec<char> = token.chars().collect();
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c == '_' {
            if !cur.is_empty() {
                parts.push(std::mem::take(&mut cur));
            }
            continue;
        }
        if let Some(&p) = cur.chars().last().as_ref() {
            let next = chars.get(i + 1).copied();
            let boundary = (p.is_lowercase() && c.is_uppercase())
                || (p.is_alphabetic() && c.is_ascii_digit())
                || (p.is_ascii_digit() && c.is_alphabetic())
                // `HTTPServer`: the last capital of a run starts a word.
                || (p.is_uppercase() && c.is_uppercase() && next.is_some_and(char::is_lowercase));
            if boundary {
                parts.push(std::mem::take(&mut cur));
            }
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts.into_iter().map(|p| p.to_lowercase()).collect()
}

/// Combining marks count as word characters in Unicode `\w`.
fn is_mark(c: char) -> bool {
    matches!(c as u32, 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F)
}

// ---- graph reads ---------------------------------------------------

impl ProjectStore {
    /// Entities whose name or qualified name equals `q`, as full rows
    /// (exact match only).
    pub fn symbol_entities(&self, build_id: &str, q: &str) -> Result<Vec<EntityRow>> {
        self.query_rows(
            "symbol entities",
            &format!(
                "SELECT {ENTITY_COLUMNS} FROM entities
                 WHERE build_id = ?1 AND (name = ?2 OR qualified_name = ?2)
                 ORDER BY qualified_name, file_id, start_line, id"
            ),
            &[&build_id, &q],
            entity_from_row,
        )
    }

    /// Edges of `relationship_type` leaving (`outgoing`) or entering
    /// (`!outgoing`) `entity_id`, ordered by id, at most `limit`.
    pub fn entity_edges(
        &self,
        build_id: &str,
        entity_id: &str,
        relationship_type: Option<&str>,
        outgoing: bool,
        limit: i64,
    ) -> Result<Vec<RelationshipRow>> {
        if !self.graph_visible(build_id)? {
            return Ok(Vec::new());
        }
        let column = if outgoing {
            "source_entity_id"
        } else {
            "target_entity_id"
        };
        let (filter, order) = match relationship_type {
            Some(_) => ("AND relationship_type = ?3", "id"),
            None => ("AND (?3 IS NULL)", "relationship_type, id"),
        };
        self.query_rows(
            "entity edges",
            &format!(
                "SELECT {RELATIONSHIP_COLUMNS} FROM relationships
                 WHERE build_id = ?1 AND {column} = ?2 {filter}
                 ORDER BY {order} LIMIT ?4"
            ),
            &[&build_id, &entity_id, &relationship_type, &limit],
            relationship_from_row,
        )
    }

    /// Unresolved edges recorded only under `symbol` (no target entity).
    pub fn unresolved_edges(
        &self,
        build_id: &str,
        symbol: &str,
        relationship_type: Option<&str>,
        limit: i64,
    ) -> Result<Vec<RelationshipRow>> {
        if !self.graph_visible(build_id)? {
            return Ok(Vec::new());
        }
        let (filter, order) = match relationship_type {
            Some(_) => ("AND relationship_type = ?3", "id"),
            None => ("AND (?3 IS NULL)", "relationship_type, id"),
        };
        self.query_rows(
            "unresolved edges",
            &format!(
                "SELECT {RELATIONSHIP_COLUMNS} FROM relationships
                 WHERE build_id = ?1 AND target_entity_id IS NULL AND target_symbol = ?2 {filter}
                 ORDER BY {order} LIMIT ?4"
            ),
            &[&build_id, &symbol, &relationship_type, &limit],
            relationship_from_row,
        )
    }

    /// Edges of many entities in one statement per
    /// [`crate::graph::LOOKUP_BATCH`] ids (see
    /// [`crate::read::KnowledgeRead::entity_edges_many`]): a window
    /// function keeps, per entity (and type when `types` is given), the
    /// first `limit` rows in exactly [`Self::entity_edges`]' order.
    pub fn entity_edges_many(
        &self,
        build_id: &str,
        entity_ids: &[String],
        types: &[&str],
        outgoing: bool,
        limit: i64,
    ) -> Result<std::collections::HashMap<String, Vec<RelationshipRow>>> {
        let column = if outgoing {
            "source_entity_id"
        } else {
            "target_entity_id"
        };
        self.edges_many(build_id, entity_ids, types, column, "", limit)
    }

    /// Unresolved edges of many symbols at once (see
    /// [`Self::entity_edges_many`]).
    pub fn unresolved_edges_many(
        &self,
        build_id: &str,
        symbols: &[String],
        types: &[&str],
        limit: i64,
    ) -> Result<std::collections::HashMap<String, Vec<RelationshipRow>>> {
        self.edges_many(
            build_id,
            symbols,
            types,
            "target_symbol",
            "AND target_entity_id IS NULL",
            limit,
        )
    }

    fn edges_many(
        &self,
        build_id: &str,
        keys: &[String],
        types: &[&str],
        column: &str,
        extra: &str,
        limit: i64,
    ) -> Result<std::collections::HashMap<String, Vec<RelationshipRow>>> {
        let mut out: std::collections::HashMap<String, Vec<RelationshipRow>> = Default::default();
        if keys.is_empty() || !self.graph_visible(build_id)? {
            return Ok(out);
        }
        let mut uniq: Vec<&String> = keys.iter().collect();
        uniq.sort();
        uniq.dedup();
        let (partition, type_filter) = if types.is_empty() {
            (
                format!("PARTITION BY {column} ORDER BY relationship_type, id"),
                String::new(),
            )
        } else {
            let marks = (0..types.len())
                .map(|i| format!("?{}", i + 3))
                .collect::<Vec<_>>()
                .join(",");
            (
                format!("PARTITION BY {column}, relationship_type ORDER BY id"),
                format!("AND relationship_type IN ({marks})"),
            )
        };
        for batch in uniq.chunks(crate::graph::LOOKUP_BATCH) {
            let first = 3 + types.len();
            let marks = (0..batch.len())
                .map(|i| format!("?{}", i + first))
                .collect::<Vec<_>>()
                .join(",");
            let mut args: Vec<&dyn rusqlite::ToSql> = vec![&build_id, &limit];
            args.extend(types.iter().map(|t| t as &dyn rusqlite::ToSql));
            args.extend(batch.iter().map(|k| *k as &dyn rusqlite::ToSql));
            let rows = self.query_rows(
                "edges many",
                &format!(
                    "SELECT {RELATIONSHIP_COLUMNS} FROM (
                        SELECT *, ROW_NUMBER() OVER ({partition}) AS rn FROM relationships
                        WHERE build_id = ?1 AND {column} IN ({marks}) {type_filter} {extra}
                     ) WHERE rn <= ?2"
                ),
                &args,
                relationship_from_row,
            )?;
            for r in rows {
                let key = match column {
                    "source_entity_id" => r.source_entity_id.clone(),
                    "target_entity_id" => r.target_entity_id.clone().unwrap_or_default(),
                    _ => r.target_symbol.clone().unwrap_or_default(),
                };
                out.entry(key).or_default().push(r);
            }
        }
        Ok(out)
    }

    /// Entities of many ids of `build_id`.
    pub fn entities_by_ids(
        &self,
        build_id: &str,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, EntityRow>> {
        let mut out = std::collections::HashMap::new();
        let mut uniq: Vec<&String> = ids.iter().collect();
        uniq.sort();
        uniq.dedup();
        for batch in uniq.chunks(crate::graph::LOOKUP_BATCH) {
            let marks = (0..batch.len())
                .map(|i| format!("?{}", i + 2))
                .collect::<Vec<_>>()
                .join(",");
            let mut args: Vec<&dyn rusqlite::ToSql> = vec![&build_id];
            args.extend(batch.iter().map(|k| *k as &dyn rusqlite::ToSql));
            for e in self.query_rows(
                "entities by ids",
                &format!(
                    "SELECT {ENTITY_COLUMNS} FROM entities WHERE build_id = ?1 AND id IN ({marks})"
                ),
                &args,
                entity_from_row,
            )? {
                out.insert(e.id.clone(), e);
            }
        }
        Ok(out)
    }

    /// Source-relative paths of many files of `build_id`.
    pub fn file_rel_paths(
        &self,
        build_id: &str,
        file_ids: &[String],
    ) -> Result<std::collections::HashMap<String, String>> {
        let mut out = std::collections::HashMap::new();
        let mut uniq: Vec<&String> = file_ids.iter().collect();
        uniq.sort();
        uniq.dedup();
        for batch in uniq.chunks(crate::graph::LOOKUP_BATCH) {
            let marks = (0..batch.len())
                .map(|i| format!("?{}", i + 2))
                .collect::<Vec<_>>()
                .join(",");
            let mut args: Vec<&dyn rusqlite::ToSql> = vec![&build_id];
            args.extend(batch.iter().map(|k| *k as &dyn rusqlite::ToSql));
            out.extend(self.query_rows(
                "file paths",
                &format!("SELECT id, rel_path FROM files WHERE build_id = ?1 AND id IN ({marks})"),
                &args,
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )?);
        }
        Ok(out)
    }

    /// Source-relative path of one file of `build_id`.
    pub fn file_rel_path(&self, build_id: &str, file_id: &str) -> Result<Option<String>> {
        Ok(self
            .query_rows(
                "file path",
                "SELECT rel_path FROM files WHERE build_id = ?1 AND id = ?2",
                &[&build_id, &file_id],
                |r| r.get(0),
            )?
            .pop())
    }
}

/// A cross-domain link of one entity, with its document's path and the
/// pinned chunk's location (when it names one).
#[derive(Debug, Clone, PartialEq)]
pub struct EntityLinkRow {
    pub link_type: String,
    pub document_id: String,
    pub chunk_id: Option<String>,
    pub resolver: String,
    pub confidence: String,
    /// Source-relative path of the document's file (document id if missing).
    pub document_path: String,
    pub chunk_page_start: Option<i64>,
    pub chunk_heading_path: Vec<String>,
}

impl ProjectStore {
    /// Links of `entity_id` in `build_id`, strongest first: confidence,
    /// then the linker's resolver order (exact, qualified, alias, filename,
    /// route), then link type. (This keeps the pick among same-time links
    /// deterministic.)
    pub fn entity_links(&self, build_id: &str, entity_id: &str) -> Result<Vec<EntityLinkRow>> {
        if !self.graph_visible(build_id)? {
            return Ok(Vec::new());
        }
        self.query_rows(
            "entity links",
            "SELECT l.link_type, l.document_id, l.chunk_id, l.resolver, l.confidence,
                    COALESCE(f.rel_path, l.document_id), c.page_start, c.heading_path
             FROM cross_links l
             LEFT JOIN documents d ON d.build_id = l.build_id AND d.id = l.document_id
             LEFT JOIN files f ON f.build_id = d.build_id AND f.id = d.file_id
             LEFT JOIN chunks c ON c.build_id = l.build_id AND c.id = l.chunk_id
             WHERE l.build_id = ?1 AND l.entity_id = ?2 AND d.id IS NOT NULL
             ORDER BY CASE l.confidence WHEN 'exact' THEN 0 WHEN 'high' THEN 1
                          WHEN 'medium' THEN 2 ELSE 3 END,
                      CASE l.resolver WHEN 'linker:exact_identifier' THEN 0
                          WHEN 'linker:qualified_identifier' THEN 1 WHEN 'linker:alias' THEN 2
                          WHEN 'linker:filename' THEN 3 WHEN 'linker:route_heuristic' THEN 4
                          ELSE 5 END,
                      l.link_type, l.id",
            &[&build_id, &entity_id],
            |r| {
                let hp: Option<String> = r.get(7)?;
                Ok(EntityLinkRow {
                    link_type: r.get(0)?,
                    document_id: r.get(1)?,
                    chunk_id: r.get(2)?,
                    resolver: r.get(3)?,
                    confidence: r.get(4)?,
                    document_path: r.get(5)?,
                    chunk_page_start: r.get(6)?,
                    chunk_heading_path: hp
                        .and_then(|h| serde_json::from_str(&h).ok())
                        .unwrap_or_default(),
                })
            },
        )
    }

    /// Attachment provenance of each of `document_ids` (one statement per
    /// [`crate::graph::LOOKUP_BATCH`] ids).
    pub fn document_attachments(
        &self,
        build_id: &str,
        document_ids: &[String],
    ) -> Result<std::collections::HashMap<String, Option<AttachmentProvenance>>> {
        let mut out = std::collections::HashMap::new();
        let mut ids: Vec<&String> = document_ids.iter().collect();
        ids.sort();
        ids.dedup();
        for batch in ids.chunks(crate::graph::LOOKUP_BATCH) {
            let marks = vec!["?"; batch.len()].join(", ");
            let mut params: Vec<&dyn rusqlite::ToSql> = vec![&build_id];
            for id in batch {
                params.push(*id);
            }
            let rows = self.query_rows(
                "document attachments",
                &format!(
                    "SELECT d.id, {ATTACHMENT_COLUMNS} FROM documents d
                     LEFT JOIN documents p ON p.build_id = d.build_id AND p.id = d.parent_document_id
                     WHERE d.build_id = ?1 AND d.id IN ({marks})"
                ),
                &params,
                |r| Ok((r.get::<_, String>(0)?, attachment(r, 1)?)),
            )?;
            out.extend(rows);
        }
        Ok(out)
    }

    /// Attachment provenance of a document (`None` for a top-level one).
    pub fn document_attachment(
        &self,
        build_id: &str,
        document_id: &str,
    ) -> Result<Option<AttachmentProvenance>> {
        let rows = self.query_rows(
            "document attachment",
            &format!(
                "SELECT {ATTACHMENT_COLUMNS} FROM documents d
                 LEFT JOIN documents p ON p.build_id = d.build_id AND p.id = d.parent_document_id
                 WHERE d.build_id = ?1 AND d.id = ?2"
            ),
            &[&build_id, &document_id],
            |r| attachment(r, 0),
        )?;
        Ok(rows.into_iter().next().flatten())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_tokens_split_on_non_word_chars() {
        assert_eq!(
            word_tokens("settlement.BetSettled"),
            ["settlement", "BetSettled"]
        );
        assert_eq!(word_tokens("my-helpers.v9"), ["my", "helpers", "v9"]);
        assert_eq!(
            word_tokens("café_total Λογαριασμός"),
            ["café_total", "Λογαριασμός"]
        );
        assert!(word_tokens("(( )) $$$").is_empty());
        assert_eq!(
            word_tokens("日本語のドキュメント"),
            ["日本語のドキュメント"]
        );
    }

    #[test]
    fn identifiers_split_into_words() {
        assert_eq!(split_identifier("ProjectStore"), ["project", "store"]);
        assert_eq!(
            split_identifier("embedding_digest"),
            ["embedding", "digest"]
        );
        assert_eq!(split_identifier("HTTPServer2"), ["http", "server", "2"]);
        assert_eq!(split_identifier("cancelOrder"), ["cancel", "order"]);
        assert_eq!(split_identifier("plain"), ["plain"]);
        assert_eq!(
            identifier_words("fn embedding_digest(store: &ProjectStore) plain"),
            "project store"
        );
        assert_eq!(identifier_words("nothing to split"), "");
    }

    #[test]
    fn like_literal_escapes_wildcards() {
        assert_eq!(like_literal("a_b%c\\d"), "a\\_b\\%c\\\\d");
    }
}
