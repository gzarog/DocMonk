//! Implementation passages (ADR 0037): retrieval text over the exact source
//! lines of a code file, kept apart from its symbols and relationships.
//!
//! * A **body** passage covers one function or method: the documentation
//!   comment and attributes above it, its signature and its implementation,
//!   owned by that entity. A body longer than [`MAX_PASSAGE_LINES`] lines
//!   or [`MAX_PASSAGE_CHARS`] characters is split at its top-level
//!   statement boundaries; a single statement that is still too long is cut
//!   into overlapping line windows.
//! * A **text** passage is a line window of a code file without symbol
//!   extraction: an extension without a grammar, or a file whose syntax
//!   errors stop extraction. It claims no symbol and no relationship.
//!
//! Passages never span a whole large file, and a file never has more than
//! [`MAX_PASSAGES_PER_FILE`] of them. Their text is the source as written
//! (only line endings are normalized), so a citation can show it verbatim.

use docmonk_core::ids::record;
use docmonk_core::models::EntityType;
use docmonk_storage::knowledge::PassageRow;

use crate::extract::Extraction;

/// Derivation identity of passages; part of the code derivation.
pub const PASSAGE_DERIVATION_VERSION: &str = "passages-1";

/// Most lines in one passage.
pub const MAX_PASSAGE_LINES: usize = 40;
/// Most characters in one passage (a single longer line is kept as its
/// prefix).
pub const MAX_PASSAGE_CHARS: usize = 2400;
/// Lines repeated between consecutive line windows.
pub const WINDOW_OVERLAP_LINES: usize = 5;
/// Most documentation-comment and attribute lines read above a definition.
pub const MAX_DOC_COMMENT_LINES: usize = 40;
/// Most passages one file contributes.
pub const MAX_PASSAGES_PER_FILE: usize = 256;

/// Why a file has text passages instead of body passages.
pub const EXTRACTION_PARSED: &str = "parsed";
pub const EXTRACTION_UNSUPPORTED: &str = "unsupported_language";
pub const EXTRACTION_PARSE_FAILED: &str = "parse_failed";

/// The file's lines (line endings normalized), 1-based through [`Lines::get`].
struct Lines<'a> {
    lines: Vec<&'a str>,
}

impl<'a> Lines<'a> {
    fn new(text: &'a str) -> Self {
        let mut lines: Vec<&str> = text
            .split('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l))
            .collect();
        // A trailing newline does not start another line.
        if lines.len() > 1 && lines.last() == Some(&"") {
            lines.pop();
        }
        Self { lines }
    }

    fn len(&self) -> i64 {
        self.lines.len() as i64
    }

    fn get(&self, line: i64) -> &'a str {
        usize::try_from(line - 1)
            .ok()
            .and_then(|i| self.lines.get(i))
            .copied()
            .unwrap_or("")
    }

    /// Characters of `start..=end` joined by `\n`.
    fn chars(&self, start: i64, end: i64) -> usize {
        (start..=end)
            .map(|l| self.get(l).chars().count() + 1)
            .sum::<usize>()
            .saturating_sub(1)
    }

    /// `start..=end` joined by `\n`, capped at [`MAX_PASSAGE_CHARS`].
    fn text(&self, start: i64, end: i64) -> String {
        let joined = (start..=end)
            .map(|l| self.get(l))
            .collect::<Vec<_>>()
            .join("\n");
        if joined.chars().count() <= MAX_PASSAGE_CHARS {
            joined
        } else {
            joined.chars().take(MAX_PASSAGE_CHARS).collect()
        }
    }
}

/// Whether `start..=end` fits one passage.
fn fits(lines: &Lines<'_>, start: i64, end: i64) -> bool {
    end - start < MAX_PASSAGE_LINES as i64 && lines.chars(start, end) <= MAX_PASSAGE_CHARS
}

/// Bounded, overlapping line windows over `start..=end`: each at most
/// [`MAX_PASSAGE_LINES`] lines and [`MAX_PASSAGE_CHARS`] characters (one
/// over-long line is a window of its own), consecutive windows sharing
/// [`WINDOW_OVERLAP_LINES`] lines.
fn windows(lines: &Lines<'_>, start: i64, end: i64) -> Vec<(i64, i64)> {
    let mut out = Vec::new();
    let mut s = start;
    while s <= end {
        let mut e = s;
        while e < end && fits(lines, s, e + 1) {
            e += 1;
        }
        out.push((s, e));
        if e >= end {
            break;
        }
        // Overlap, but always move forward.
        s = (e + 1 - WINDOW_OVERLAP_LINES as i64).max(s + 1);
    }
    out
}

/// Line prefixes that continue a definition's documentation upwards.
fn comment_markers(language: &str) -> (&'static [&'static str], &'static [&'static str]) {
    // (comment prefixes, attribute / decorator prefixes)
    match language {
        "python" => (&["#"], &["@"]),
        "rust" => (&["//", "/*", "*"], &["#["]),
        "csharp" => (&["//", "/*", "*"], &["["]),
        "go" => (&["//", "/*", "*"], &[]),
        _ => (&["//", "/*", "*"], &["@"]),
    }
}

/// The first line of the documentation comments and attributes directly
/// above `start_line` (no blank line in between), or `start_line`.
fn doc_start(lines: &Lines<'_>, language: &str, start_line: i64) -> i64 {
    let (comments, attributes) = comment_markers(language);
    let mut first = start_line;
    let mut l = start_line - 1;
    while l >= 1 && start_line - l <= MAX_DOC_COMMENT_LINES as i64 {
        let t = lines.get(l).trim_start();
        let doc = comments.iter().any(|m| t.starts_with(m))
            || attributes.iter().any(|m| t.starts_with(m));
        if !doc || t.is_empty() {
            break;
        }
        first = l;
        l -= 1;
    }
    first
}

/// Parts of one body `start..=end`: whole statement groups while they fit,
/// line windows for a statement that does not fit alone.
fn body_parts(lines: &Lines<'_>, start: i64, end: i64, statements: &[i64]) -> Vec<(i64, i64)> {
    if fits(lines, start, end) {
        return vec![(start, end)];
    }
    // Segments between consecutive split points (the first holds the
    // documentation and the signature).
    let mut points: Vec<i64> = vec![start];
    points.extend(
        statements
            .iter()
            .copied()
            .filter(|l| *l > start && *l <= end),
    );
    points.dedup();
    let segments: Vec<(i64, i64)> = points
        .iter()
        .enumerate()
        .map(|(i, &s)| (s, points.get(i + 1).map_or(end, |n| n - 1)))
        .filter(|(s, e)| s <= e)
        .collect();
    let mut out = Vec::new();
    let mut cur: Option<(i64, i64)> = None;
    for (s, e) in segments {
        match cur {
            Some((cs, _)) if fits(lines, cs, e) => cur = Some((cs, e)),
            _ => {
                if let Some(c) = cur.take() {
                    out.push(c);
                }
                if fits(lines, s, e) {
                    cur = Some((s, e));
                } else {
                    out.extend(windows(lines, s, e));
                }
            }
        }
    }
    out.extend(cur);
    out
}

/// Body passages of every function and method of a parsed file, in file
/// order, at most [`MAX_PASSAGES_PER_FILE`].
pub fn body_passages(
    file_id: &str,
    language: &str,
    source: &str,
    extraction: &Extraction,
    ids: &[String],
) -> Vec<PassageRow> {
    let lines = Lines::new(source);
    let statements = |local: usize| -> &[i64] {
        extraction
            .bodies
            .iter()
            .find(|b| b.entity_local_id == local)
            .map_or(&[][..], |b| b.statement_lines.as_slice())
    };
    let mut owners: Vec<&crate::extract::ExtractedEntity> = extraction
        .entities
        .iter()
        .filter(|e| matches!(e.kind, EntityType::Function | EntityType::Method))
        .collect();
    owners.sort_by_key(|e| (e.start_line, e.start_col, e.local_id));
    let mut out = Vec::new();
    for e in owners {
        let start = doc_start(&lines, language, e.start_line);
        let end = e.end_line.min(lines.len()).max(start);
        let parts = body_parts(&lines, start, end, statements(e.local_id));
        let header = match &e.signature {
            Some(sig) if !sig.trim().is_empty() => format!("{} {}", e.qualified_name, sig.trim()),
            _ => e.qualified_name.clone(),
        };
        let n = parts.len() as i64;
        for (part, (s, t)) in parts.into_iter().enumerate() {
            if out.len() >= MAX_PASSAGES_PER_FILE {
                return out;
            }
            let ordinal = out.len() as i64;
            out.push(PassageRow {
                id: record::passage_id(file_id, ordinal as u64),
                file_id: file_id.to_owned(),
                entity_id: Some(ids[e.local_id].clone()),
                kind: "body".into(),
                extraction: EXTRACTION_PARSED.into(),
                language: Some(language.to_owned()),
                ordinal,
                part: part as i64,
                parts: n,
                start_line: s,
                end_line: t,
                header: header.clone(),
                text: lines.text(s, t),
            });
        }
    }
    out
}

/// Text passages over a whole file (no symbols): overlapping line windows,
/// blank windows skipped, at most [`MAX_PASSAGES_PER_FILE`].
pub fn text_passages(
    file_id: &str,
    rel_path: &str,
    language: Option<&str>,
    extraction: &str,
    source: &str,
) -> Vec<PassageRow> {
    let lines = Lines::new(source);
    if lines.len() == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (s, e) in windows(&lines, 1, lines.len()) {
        if out.len() >= MAX_PASSAGES_PER_FILE {
            break;
        }
        let text = lines.text(s, e);
        if text.trim().is_empty() {
            continue;
        }
        let ordinal = out.len() as i64;
        out.push(PassageRow {
            id: record::passage_id(file_id, ordinal as u64),
            file_id: file_id.to_owned(),
            entity_id: None,
            kind: "text".into(),
            extraction: extraction.to_owned(),
            language: language.map(str::to_owned),
            ordinal,
            part: 0,
            parts: 1,
            start_line: s,
            end_line: e,
            header: rel_path.to_owned(),
            text,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_are_bounded_and_overlap() {
        let src: String = (1..=100).map(|i| format!("line {i}\n")).collect();
        let lines = Lines::new(&src);
        assert_eq!(lines.len(), 100);
        let w = windows(&lines, 1, 100);
        assert_eq!(w[0], (1, 40));
        assert_eq!(w[1].0, 36, "five lines of overlap");
        assert_eq!(w.last().unwrap().1, 100);
        assert!(w.iter().all(|(s, e)| e - s < MAX_PASSAGE_LINES as i64));
        // An over-long line is one window of its own, kept as its prefix.
        let long = format!("a\n{}\nb\n", "x".repeat(10_000));
        let lines = Lines::new(&long);
        let w = windows(&lines, 1, 3);
        assert_eq!(w, [(1, 1), (2, 2), (3, 3)]);
        assert_eq!(lines.text(2, 2).chars().count(), MAX_PASSAGE_CHARS);
    }

    #[test]
    fn doc_comments_and_attributes_join_the_definition() {
        let src = "use x;\n\n/// Adds.\n/// Twice.\n#[inline]\nfn add() {}\n";
        assert_eq!(doc_start(&Lines::new(src), "rust", 6), 3);
        let src = "x = 1\n# not attached\n\n@cached\ndef f():\n    pass\n";
        assert_eq!(doc_start(&Lines::new(src), "python", 5), 4);
        let src = "class A {\n  /** Doc. */\n  run() {}\n}\n";
        assert_eq!(doc_start(&Lines::new(src), "typescript", 3), 2);
    }

    #[test]
    fn large_bodies_split_at_statements() {
        let mut src = String::from("fn big() {\n");
        for i in 0..30 {
            src.push_str(&format!("    let a{i} = {i};\n    let b{i} = a{i};\n"));
        }
        src.push_str("}\n");
        let lines = Lines::new(&src);
        let statements: Vec<i64> = (2..=61).collect();
        let parts = body_parts(&lines, 1, 62, &statements);
        assert!(parts.len() >= 2, "{parts:?}");
        assert_eq!(parts[0].0, 1);
        assert_eq!(parts.last().unwrap().1, 62);
        // Statement-aligned parts tile the body without gaps or overlap.
        for w in parts.windows(2) {
            assert_eq!(w[0].1 + 1, w[1].0);
        }
        assert!(parts.iter().all(|(s, e)| fits(&lines, *s, *e)));
    }

    #[test]
    fn text_passages_cover_the_file_without_symbols() {
        let src = "def a(:\n  pass\n";
        let p = text_passages("f", "a.py", Some("python"), EXTRACTION_PARSE_FAILED, src);
        assert_eq!(p.len(), 1);
        assert_eq!((p[0].start_line, p[0].end_line), (1, 2));
        assert_eq!(p[0].entity_id, None);
        assert_eq!(p[0].kind, "text");
        assert_eq!(p[0].text, "def a(:\n  pass");
        assert!(text_passages("f", "a.rb", None, EXTRACTION_UNSUPPORTED, "").is_empty());
        let huge: String = (0..100_000).map(|i| format!("l{i}\n")).collect();
        let p = text_passages("f", "a.rb", None, EXTRACTION_UNSUPPORTED, &huge);
        assert_eq!(p.len(), MAX_PASSAGES_PER_FILE);
    }
}
