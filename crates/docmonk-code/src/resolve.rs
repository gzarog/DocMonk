//! Reference resolution.
//!
//! EXACT: one same-file match (qualified, then bare name). HIGH: several
//! same-file name matches, or a cross-file exact qualified-name match.
//! MEDIUM: a cross-file bare-name match, or nothing (kept as an
//! unresolved `target_symbol`). HEURISTIC is reserved for framework rules.

use docmonk_core::models::Confidence;

/// The fields resolution looks at.
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    pub id: &'a str,
    pub file_id: &'a str,
    pub name: &'a str,
    pub qualified_name: &'a str,
    pub start_line: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    pub entity_id: Option<String>,
    pub symbol: String,
    pub confidence: Confidence,
    pub resolver: &'static str,
}

/// `text.rsplit(".", 1)[-1]`.
pub fn bare_name(text: &str) -> &str {
    text.rsplit('.').next().unwrap_or(text)
}

/// Same-file stage only; `None` means "consult other files".
pub fn resolve_same_file(text: &str, same_file: &[Candidate<'_>]) -> Option<ResolvedTarget> {
    let bare = bare_name(text);
    if bare.is_empty() {
        return Some(ResolvedTarget {
            entity_id: None,
            symbol: text.to_owned(),
            confidence: Confidence::Medium,
            resolver: "unresolved",
        });
    }
    let by_qn: Vec<_> = same_file
        .iter()
        .filter(|e| e.qualified_name == text)
        .collect();
    if by_qn.len() == 1 {
        return Some(found(
            by_qn[0].id,
            text,
            Confidence::Exact,
            "same_file_qualified",
        ));
    }
    let mut by_name: Vec<_> = same_file.iter().filter(|e| e.name == bare).collect();
    by_name.sort_by(|a, b| a.qualified_name.cmp(b.qualified_name));
    match by_name.len() {
        0 => None,
        1 => Some(found(
            by_name[0].id,
            text,
            Confidence::Exact,
            "same_file_name",
        )),
        _ => Some(found(
            by_name[0].id,
            text,
            Confidence::High,
            "same_file_name_ambiguous",
        )),
    }
}

/// Cross-file stage over candidates that exclude the referencing file.
pub fn resolve_cross_file(
    text: &str,
    by_qualified: &[Candidate<'_>],
    by_name: &[Candidate<'_>],
) -> ResolvedTarget {
    let bare = bare_name(text);
    let mut qn: Vec<_> = by_qualified.to_vec();
    qn.sort_by(|a, b| (a.file_id, a.start_line).cmp(&(b.file_id, b.start_line)));
    if let Some(first) = qn.first() {
        let resolver = if qn.len() == 1 {
            "cross_file_qualified"
        } else {
            "cross_file_qualified_ambiguous"
        };
        return found(first.id, text, Confidence::High, resolver);
    }
    let mut named: Vec<_> = by_name.to_vec();
    named.sort_by(|a, b| (a.qualified_name, a.file_id).cmp(&(b.qualified_name, b.file_id)));
    if let Some(first) = named.first() {
        return found(first.id, bare, Confidence::Medium, "name_only");
    }
    ResolvedTarget {
        entity_id: None,
        symbol: bare.to_owned(),
        confidence: Confidence::Medium,
        resolver: "unresolved",
    }
}

fn found(id: &str, symbol: &str, confidence: Confidence, resolver: &'static str) -> ResolvedTarget {
    ResolvedTarget {
        entity_id: Some(id.to_owned()),
        symbol: symbol.to_owned(),
        confidence,
        resolver,
    }
}

/// Full reference resolution (`resolve_reference`).
pub fn resolve_reference<'a>(
    text: &str,
    same_file: &[Candidate<'a>],
    qualified_lookup: impl Fn(&str) -> Vec<Candidate<'a>>,
    name_lookup: impl Fn(&str) -> Vec<Candidate<'a>>,
) -> ResolvedTarget {
    if let Some(r) = resolve_same_file(text, same_file) {
        return r;
    }
    resolve_cross_file(text, &qualified_lookup(text), &name_lookup(bare_name(text)))
}
