//! Bounded deterministic query decomposition (ADR 0033, P0-R02).
//!
//! Comparisons ("compare A and B …", "A vs B …", "difference between A and
//! B …"), multi-part questions ("…? …?", "…; …", "… and also …") and
//! flows ("from A through B to C") are split into at most `max`
//! subqueries. Nothing here calls a model; a query that matches no rule is
//! not decomposed. Subqueries only rephrase the user's own words, so they
//! can never reach outside the request's hard filters (which the caller
//! applies to every subquery's retrieval).

use std::sync::OnceLock;

use regex::Regex;
use serde::Serialize;

/// One subquery and why it exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Subquery {
    pub text: String,
    /// `comparison`, `part` or `hop`.
    pub role: &'static str,
}

fn re(s: &str) -> Regex {
    Regex::new(&format!("(?i){s}")).expect("valid pattern")
}

struct Rules {
    compare: Regex,
    versus: Regex,
    between: Regex,
    flow: Regex,
    also: Regex,
}

fn rules() -> &'static Rules {
    static R: OnceLock<Rules> = OnceLock::new();
    R.get_or_init(|| Rules {
        compare: re(r"^\s*compare\s+(?:the\s+)?(.+?)\s+(?:and|with|to)\s+(?:the\s+)?(.+?)(?:\s+(?:in terms of|regarding|on|for)\s+(.+))?\s*\??$"),
        versus: re(r"^\s*(.+?)\s+(?:vs\.?|versus)\s+(.+?)\s*\??$"),
        between: re(r"differences?\s+between\s+(?:the\s+)?(.+?)\s+and\s+(?:the\s+)?(.+?)(?:\s+(?:in|for|regarding|when)\s+(.+))?\s*\??$"),
        flow: re(r"from\s+([\w.]+)\s+(?:through|via)\s+([\w.]+)(?:\s+(?:and\s+)?(?:through|via)\s+([\w.]+))?\s+to\s+(?:the\s+)?([\w.]+)"),
        also: re(r"\s+(?:and also|and then also|as well as)\s+|;\s*"),
    })
}

fn clean(s: &str) -> String {
    s.trim()
        .trim_end_matches(['?', '.', '!'])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn push(out: &mut Vec<Subquery>, text: String, role: &'static str) {
    let t = clean(&text);
    if t.split_whitespace().count() >= 1 && !out.iter().any(|s| s.text.eq_ignore_ascii_case(&t)) {
        out.push(Subquery { text: t, role });
    }
}

/// Splits `query` into at most `max` subqueries; empty when the query is
/// not decomposable (it is then answered as one query).
pub fn decompose(query: &str, max: usize) -> Vec<Subquery> {
    let q = query.trim();
    let r = rules();
    let mut out = Vec::new();
    let pair = |out: &mut Vec<Subquery>, a: &str, b: &str, rest: Option<&str>| {
        let rest = rest.map(clean).filter(|x| !x.is_empty());
        for side in [a, b] {
            let text = match &rest {
                Some(rest) => format!("{} {rest}", clean(side)),
                None => clean(side),
            };
            push(out, text, "comparison");
        }
    };
    if let Some(c) = r.compare.captures(q) {
        pair(&mut out, &c[1], &c[2], c.get(3).map(|m| m.as_str()));
    } else if let Some(c) = r.between.captures(q) {
        pair(&mut out, &c[1], &c[2], c.get(3).map(|m| m.as_str()));
    } else if let Some(c) = r.versus.captures(q) {
        // "A vs B retries": the words after B apply to both sides.
        let a = clean(&c[1]);
        let b_full = clean(&c[2]);
        let mut b_words = b_full.split_whitespace();
        let b = b_words.next().unwrap_or_default().to_owned();
        let rest: Vec<&str> = b_words.collect();
        let rest = (!rest.is_empty()).then(|| rest.join(" "));
        pair(&mut out, &a, &b, rest.as_deref());
    } else if let Some(c) = r.flow.captures(q) {
        for i in 1..=4 {
            if let Some(m) = c.get(i) {
                push(&mut out, m.as_str().to_owned(), "hop");
            }
        }
    } else {
        // Multi-part questions: several "?" sentences, ";" or "and also".
        let parts: Vec<&str> = q
            .split_inclusive('?')
            .flat_map(|p| r.also.split(p))
            .map(str::trim)
            .filter(|p| p.split_whitespace().count() >= 3)
            .collect();
        if parts.len() >= 2 {
            for p in parts {
                push(&mut out, p.to_owned(), "part");
            }
        }
    }
    if out.len() < 2 {
        return Vec::new();
    }
    out.truncate(max.max(1));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(q: &str) -> Vec<String> {
        decompose(q, 4).into_iter().map(|s| s.text).collect()
    }

    #[test]
    fn comparisons_split_into_both_sides() {
        assert_eq!(
            texts("compare BillingService and LedgerService regarding retries"),
            ["BillingService retries", "LedgerService retries"]
        );
        assert_eq!(
            texts("billing vs ledger retries"),
            ["billing retries", "ledger retries"]
        );
        assert_eq!(
            texts("What are the differences between the expense policy and the travel policy?"),
            ["expense policy", "travel policy"]
        );
    }

    #[test]
    fn multi_part_and_flows() {
        assert_eq!(
            texts("How are refunds approved? Who signs off large invoices?"),
            ["How are refunds approved", "Who signs off large invoices"]
        );
        assert_eq!(
            texts("how is the ledger reconciled; how are retries scheduled"),
            ["how is the ledger reconciled", "how are retries scheduled"]
        );
        assert_eq!(
            texts("trace an order from OrderService through PaymentGateway to LedgerWriter"),
            ["OrderService", "PaymentGateway", "LedgerWriter"]
        );
    }

    #[test]
    fn single_intent_queries_are_not_decomposed_and_budget_holds() {
        for q in [
            "SettlementService",
            "how are failed payments retried",
            "",
            "a and b",
        ] {
            assert!(decompose(q, 4).is_empty(), "{q:?}");
        }
        let many = "What is A? What is B? What is C? What is D? What is E?";
        assert_eq!(decompose(many, 4).len(), 4);
        assert_eq!(decompose(many, 2).len(), 2);
        // Deterministic.
        assert_eq!(decompose(many, 4), decompose(many, 4));
    }
}
