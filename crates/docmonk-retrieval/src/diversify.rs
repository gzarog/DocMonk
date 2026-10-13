//! Duplicate control and diversity for fused evidence (ADR 0033, P0-R02).
//!
//! * [`dedup`]: one hit per stable provenance key `(source, kind, id)` and
//!   per content fingerprint (the same text indexed twice).
//! * [`collapse_siblings`]: at most `per_document_cap` chunks of one
//!   document (or file) and, optionally, `per_source_cap` hits per source,
//!   so near-identical neighbours do not crowd out other evidence. Distinct
//!   sections still appear up to the cap.
//! * [`mmr`]: opt-in maximal-marginal-relevance reordering over token
//!   overlap. Pinned exact matches always stay first, in their order.
//!
//! All three are deterministic and run after hard filtering, so they can
//! only drop or reorder in-scope hits, never add one.

use std::collections::{HashMap, HashSet};

use crate::hybrid::RankedHit;

/// Stable content fingerprint (hex) of a text, whitespace- and
/// case-normalized.
pub fn fingerprint(text: &str) -> String {
    let norm: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    // FNV-1a 64: stable across platforms and runs (DefaultHasher is not).
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in norm.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

fn text_of(h: &RankedHit) -> &str {
    h.candidate.snippet.as_deref().unwrap_or(&h.candidate.title)
}

fn group_of(h: &RankedHit) -> String {
    // Document chunks group by their file (all sections of one document);
    // entities by file too, so one class's methods do not flood results.
    format!("{}\u{0}{}", h.candidate.source_id, h.candidate.path)
}

/// Drops repeated `(source, kind, id)` keys and repeated non-empty texts,
/// keeping the first (best-ranked) occurrence. Returns how many were dropped.
pub fn dedup(hits: &mut Vec<RankedHit>) -> usize {
    let before = hits.len();
    let mut keys = HashSet::new();
    let mut texts = HashSet::new();
    hits.retain(|h| {
        let c = &h.candidate;
        if !keys.insert((c.source_id.clone(), c.kind.clone(), c.id.clone())) {
            return false;
        }
        let t = text_of(h);
        if t.split_whitespace().count() >= 4 && !texts.insert(fingerprint(t)) {
            return false;
        }
        true
    });
    before - hits.len()
}

/// Caps hits per document/file and per source (0 = no source cap). Pinned
/// exact matches are never dropped. Returns how many were collapsed.
pub fn collapse_siblings(
    hits: &mut Vec<RankedHit>,
    per_document_cap: usize,
    per_source_cap: usize,
) -> usize {
    let before = hits.len();
    let mut per_doc: HashMap<String, usize> = HashMap::new();
    let mut per_src: HashMap<String, usize> = HashMap::new();
    hits.retain(|h| {
        let pinned = h.candidate.exact_match;
        let d = per_doc.entry(group_of(h)).or_default();
        let s = per_src.entry(h.candidate.source_id.clone()).or_default();
        let keep = pinned
            || (*d < per_document_cap.max(1) && (per_source_cap == 0 || *s < per_source_cap));
        if keep {
            *d += 1;
            *s += 1;
        }
        keep
    });
    before - hits.len()
}

fn tokens(t: &str) -> HashSet<String> {
    t.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3)
        .map(str::to_lowercase)
        .collect()
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f64;
    inter / (a.len() + b.len()) as f64 * 2.0
}

/// MMR reorder: `lambda` weighs the original rank (1.0 = unchanged order)
/// against novelty w.r.t. what is already selected. Pinned hits first.
pub fn mmr(hits: Vec<RankedHit>, lambda: f64) -> Vec<RankedHit> {
    let (mut out, rest): (Vec<RankedHit>, Vec<RankedHit>) =
        hits.into_iter().partition(|h| h.candidate.exact_match);
    let n = rest.len().max(1) as f64;
    let mut pool: Vec<(usize, HashSet<String>, RankedHit)> = rest
        .into_iter()
        .enumerate()
        .map(|(i, h)| (i, tokens(text_of(&h)), h))
        .collect();
    let mut chosen: Vec<HashSet<String>> = out.iter().map(|h| tokens(text_of(h))).collect();
    while !pool.is_empty() {
        let mut best = 0;
        let mut best_score = f64::MIN;
        for (j, (rank, toks, _)) in pool.iter().enumerate() {
            let relevance = 1.0 - *rank as f64 / n;
            let redundancy = chosen.iter().map(|c| jaccard(toks, c)).fold(0.0, f64::max);
            let score = lambda * relevance - (1.0 - lambda) * redundancy;
            if score > best_score + 1e-12 {
                best_score = score;
                best = j;
            }
        }
        let (_, toks, h) = pool.remove(best);
        chosen.push(toks);
        out.push(h);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hybrid::Candidate;
    use crate::lexical::LexicalTier;

    fn hit(id: &str, src: &str, path: &str, text: &str, pinned: bool) -> RankedHit {
        RankedHit {
            candidate: Candidate {
                kind: "document".into(),
                id: id.into(),
                title: id.into(),
                path: path.into(),
                source_id: src.into(),
                snippet: Some(text.into()),
                location: None,
                lexical_tier: None,
                lexical_fts_rank: 0,
                lexical_query_tier: LexicalTier::Phrase,
                entity_kind_rank: 99,
                mtime: 0.0,
                lexical_rank: None,
                bm25: None,
                semantic_score: None,
                semantic_rank: None,
                exact_match: pinned,
                rrf_score: None,
            },
            tier_label: "hybrid",
            neural_score: None,
        }
    }

    fn ids(v: &[RankedHit]) -> Vec<&str> {
        v.iter().map(|h| h.candidate.id.as_str()).collect()
    }

    #[test]
    fn dedup_by_key_and_content() {
        let mut v = vec![
            hit(
                "a",
                "s",
                "x.md",
                "retry the settlement nightly batch",
                false,
            ),
            hit("a", "s", "x.md", "other", false),
            hit(
                "b",
                "s",
                "y.md",
                "Retry  the settlement nightly batch",
                false,
            ),
            hit("c", "t", "x.md", "short", false),
        ];
        assert_eq!(dedup(&mut v), 2);
        assert_eq!(ids(&v), ["a", "c"]);
        assert_eq!(fingerprint("A  b"), fingerprint("a b"));
    }

    #[test]
    fn sibling_and_source_caps_keep_pins() {
        let mut v = vec![
            hit("p", "s", "x.md", "pinned", true),
            hit("1", "s", "x.md", "one", false),
            hit("2", "s", "x.md", "two", false),
            hit("3", "s", "x.md", "three", false),
            hit("4", "s", "y.md", "four", false),
            hit("5", "t", "z.md", "five", false),
        ];
        collapse_siblings(&mut v, 2, 0);
        assert_eq!(ids(&v), ["p", "1", "4", "5"]);
        let mut w = v.clone();
        collapse_siblings(&mut w, 9, 2);
        assert_eq!(ids(&w), ["p", "1", "5"]);
    }

    #[test]
    fn mmr_promotes_novel_evidence_but_keeps_pins_first() {
        let v = vec![
            hit("dup1", "s", "a.md", "billing retry policy nightly", false),
            hit("pin", "s", "p.cs", "BillingService", true),
            hit(
                "dup2",
                "s",
                "b.md",
                "billing retry policy nightly window",
                false,
            ),
            hit("novel", "t", "c.md", "ledger reconciliation journal", false),
        ];
        let out = mmr(v.clone(), 0.5);
        assert_eq!(ids(&out)[0], "pin");
        assert_eq!(ids(&out), ["pin", "dup1", "novel", "dup2"]);
        // lambda 1.0 keeps the original order (after pins).
        assert_eq!(ids(&mmr(v, 1.0)), ["pin", "dup1", "dup2", "novel"]);
    }
}
