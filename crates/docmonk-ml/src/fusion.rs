//! Reciprocal Rank Fusion.
//! The hybrid search pipeline builds on these.

/// RRF smoothing constant: `1 / (k + rank)`.
pub const RRF_K: u32 = 60;
pub const MAX_LEXICAL_CANDIDATES: usize = 50;
pub const MAX_SEMANTIC_CANDIDATES: usize = 50;
pub const MAX_FUSION_CANDIDATES: usize = 100;

/// `1/(k + lexical_rank) + 1/(k + semantic_rank)`, each term only when that
/// 1-based rank is known (`None` = not found by that signal).
pub fn rrf_score(lexical_rank: Option<u32>, semantic_rank: Option<u32>, k: u32) -> f64 {
    let term = |r: Option<u32>| r.map_or(0.0, |r| 1.0 / f64::from(k + r));
    term(lexical_rank) + term(semantic_rank)
}

/// Fuses two best-first ranked lists of ids into one RRF-ordered list
/// (ties by id), capped at [`MAX_FUSION_CANDIDATES`].
pub fn fuse(lexical: &[String], semantic: &[String]) -> Vec<(String, f64)> {
    use std::collections::BTreeMap;
    let mut ranks: BTreeMap<&str, (Option<u32>, Option<u32>)> = BTreeMap::new();
    for (i, id) in lexical.iter().take(MAX_LEXICAL_CANDIDATES).enumerate() {
        ranks.entry(id).or_default().0.get_or_insert(i as u32 + 1);
    }
    for (i, id) in semantic.iter().take(MAX_SEMANTIC_CANDIDATES).enumerate() {
        ranks.entry(id).or_default().1.get_or_insert(i as u32 + 1);
    }
    let mut out: Vec<(String, f64)> = ranks
        .into_iter()
        .map(|(id, (l, s))| (id.to_owned(), rrf_score(l, s, RRF_K)))
        .collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out.truncate(MAX_FUSION_CANDIDATES);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_reference_formula() {
        // Expected: 1/61 + 1/62, 1/61, 0.
        assert!((rrf_score(Some(1), Some(2), 60) - (1.0 / 61.0 + 1.0 / 62.0)).abs() < 1e-15);
        assert!((rrf_score(Some(1), None, 60) - 1.0 / 61.0).abs() < 1e-15);
        assert_eq!(rrf_score(None, None, 60), 0.0);
    }

    #[test]
    fn found_by_both_signals_outranks_single_signal() {
        let lex = vec!["a".to_string(), "b".into(), "c".into()];
        let sem = vec!["c".to_string(), "d".into()];
        let fused = fuse(&lex, &sem);
        assert_eq!(fused[0].0, "c");
        assert_eq!(
            fused.iter().map(|f| f.0.as_str()).collect::<Vec<_>>(),
            ["c", "a", "b", "d"]
        );
    }
}
