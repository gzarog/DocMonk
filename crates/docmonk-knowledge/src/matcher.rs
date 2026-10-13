//! Whole-identifier matching and the inverted word index
//! used by the linker.
//!
//! A needle matches where it is neither preceded nor followed by a word
//! character or `.` (`(?<![\w.])needle(?![\w.])`), with a
//! Unicode `\w`: letters (L*), numbers (N*) and `_`. Because each
//! maximal word run inside a needle is then a whole word of any text it
//! matches, a unit lacking any of the needle's words cannot match; the index
//! only narrows candidates that the exact check then confirms, so matching is
//! never "every needle x every unit".

use std::collections::{HashMap, HashSet};

use unicode_general_category::{get_general_category, GeneralCategory as G};

/// Unicode `\w`: letters, numbers, `_` and combining marks.
pub fn is_word(c: char) -> bool {
    c == '_'
        || matches!(
            get_general_category(c),
            G::UppercaseLetter
                | G::LowercaseLetter
                | G::TitlecaseLetter
                | G::ModifierLetter
                | G::OtherLetter
                | G::DecimalNumber
                | G::LetterNumber
                | G::OtherNumber
        )
}

/// Maximal runs of word characters (`re.findall(r"\w+")`).
pub fn words(s: &str) -> Vec<&str> {
    s.split(|c: char| !is_word(c))
        .filter(|w| !w.is_empty())
        .collect()
}

fn boundary(c: Option<char>) -> bool {
    c.is_none_or(|c| !is_word(c) && c != '.')
}

/// True when `needle` occurs in `text` as a standalone identifier.
pub fn contains_identifier(text: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let mut from = 0;
    while let Some(i) = text[from..].find(needle).map(|i| i + from) {
        let end = i + needle.len();
        if boundary(text[..i].chars().next_back()) && boundary(text[end..].chars().next()) {
            return true;
        }
        // Advance one character (overlapping occurrences are still found).
        from = i + text[i..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// Inverted word index over one list of unit texts.
pub struct UnitIndex<'a> {
    texts: Vec<&'a str>,
    postings: HashMap<&'a str, Vec<usize>>,
}

impl<'a> UnitIndex<'a> {
    pub fn new(texts: Vec<&'a str>) -> Self {
        let mut postings: HashMap<&'a str, Vec<usize>> = HashMap::new();
        for (pos, text) in texts.iter().enumerate() {
            let unique: HashSet<&str> = words(text).into_iter().collect();
            for w in unique {
                postings.entry(w).or_default().push(pos);
            }
        }
        Self { texts, postings }
    }

    pub fn len(&self) -> usize {
        self.texts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.texts.is_empty()
    }

    /// Unit positions (ascending) containing every word of `needle`.
    pub fn candidates(&self, needle: &str) -> Vec<usize> {
        let needle_words: HashSet<&str> = words(needle).into_iter().collect();
        if needle_words.is_empty() {
            return (0..self.texts.len()).collect();
        }
        let mut lists = Vec::with_capacity(needle_words.len());
        for w in needle_words {
            match self.postings.get(w) {
                Some(p) if !p.is_empty() => lists.push(p),
                _ => return Vec::new(),
            }
        }
        lists.sort_by_key(|l| l.len());
        let (first, rest) = lists.split_first().expect("non-empty");
        let rest: Vec<HashSet<usize>> = rest.iter().map(|l| l.iter().copied().collect()).collect();
        first
            .iter()
            .copied()
            .filter(|p| rest.iter().all(|r| r.contains(p)))
            .collect()
    }

    /// Positions whose text contains `needle` as a standalone identifier.
    pub fn matching(&self, needle: &str) -> Vec<usize> {
        self.candidates(needle)
            .into_iter()
            .filter(|p| contains_identifier(self.texts[*p], needle))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundaries_follow_the_reference_regex() {
        assert!(contains_identifier(
            "use calculate_tax now",
            "calculate_tax"
        ));
        assert!(!contains_identifier("xcalculate_tax", "calculate_tax"));
        // A trailing "." blocks the match.
        assert!(!contains_identifier("exposes cancelOrder.", "cancelOrder"));
        assert!(contains_identifier("(Ledger) and [post_entry]", "Ledger"));
        assert!(contains_identifier("Dog.bark here", "Dog.bark"));
        assert!(!contains_identifier("a.Dog.bark here", "Dog.bark"));
        assert!(contains_identifier("με café_total και", "café_total"));
        assert!(!contains_identifier("Λογαριασμόςx", "Λογαριασμός"));
        assert!(
            contains_identifier("aab ab", "ab"),
            "later occurrence found"
        );
        assert!(!contains_identifier("anything", ""));
    }

    #[test]
    fn index_only_narrows() {
        let idx = UnitIndex::new(vec!["alpha beta", "beta gamma", "alpha.beta"]);
        assert_eq!(idx.candidates("alpha.beta"), vec![0, 2]);
        assert_eq!(idx.matching("alpha.beta"), vec![2]);
        assert_eq!(idx.candidates("..."), vec![0, 1, 2]);
        assert!(idx.matching("delta").is_empty());
    }
}
