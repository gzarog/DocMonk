//! Deterministic query classification:
//! the query's shape (diagnostic, shown by `search --explain`) and how
//! confident the lexical pass already is (`search.lazy_semantic`).

use std::sync::OnceLock;

use regex::Regex;

use crate::lexical::{RankTier, SearchResult};

fn path_hint() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"[\\/]|\.[A-Za-z0-9]{1,6}$").expect("valid regex"))
}

fn identifier() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*(\.[A-Za-z_][A-Za-z0-9_]*)*$").expect("valid regex")
    })
}

/// `symbol`, `path`, `keyword` or `conceptual`.
pub fn classify_query(query: &str) -> &'static str {
    let text = query.trim();
    if text.is_empty() {
        return "keyword";
    }
    if path_hint().is_match(text) {
        return "path";
    }
    if identifier().is_match(text) && (text.contains('.') || text != text.to_lowercase()) {
        return "symbol";
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() <= 3 && words.iter().all(|w| w.chars().count() <= 20) {
        return "keyword";
    }
    "conceptual"
}

/// `high` when any hit is an exact, qualified or alias symbol or a title
/// or heading match, `medium` for any other hit, `low` for none.
pub fn estimate_confidence(results: &[SearchResult]) -> &'static str {
    if results.is_empty() {
        return "low";
    }
    let high = results.iter().any(|r| {
        matches!(
            r.tier,
            RankTier::ExactSymbol
                | RankTier::QualifiedSymbol
                | RankTier::AliasSymbol
                | RankTier::TitleOrHeading
        )
    });
    if high {
        "high"
    } else {
        "medium"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_match_reference() {
        for (q, want) in [
            ("", "keyword"),
            ("src/app.py", "path"),
            ("report.pdf", "path"),
            ("SettlementService", "symbol"),
            ("pkg.mod.func", "path"),
            ("pkg.module_name", "symbol"),
            ("cholesterol", "keyword"),
            ("how does billing work", "conceptual"),
            ("two words", "keyword"),
        ] {
            assert_eq!(classify_query(q), want, "{q:?}");
        }
        assert_eq!(estimate_confidence(&[]), "low");
    }
}
