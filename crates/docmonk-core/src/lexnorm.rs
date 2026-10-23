//! The normalized lexical projection (ADR 0039): one deterministic,
//! storage-neutral normalization of words. Both lexical backends index it
//! next to the original text (the local FTS `words` column, the server
//! `norm` field), and queries are normalized the same way, so the two
//! backends match the same forms without relying on engine analyzers.
//!
//! The original text is never changed: citations, snippets, exact
//! identifiers and paths are read and matched as stored. The projection
//! only *adds* forms:
//!
//! * NFKC (full-width Latin and digits, ligatures) and lowercase;
//! * diacritics removed after Latin, Greek, Arabic and Hebrew letters
//!   (`é` → `e`, `ά` → `α`, harakat, hamza seats); never after kana (a
//!   dakuten changes the sound) or Han; Cyrillic loses only `ё`'s diaeresis
//!   and stress accents (`й` stays `й`);
//! * `ß` → `ss`, Greek final `ς` → `σ`;
//! * Greek: a light inflectional stemmer (`άδεια`, `άδειας`, `άδειες` →
//!   `αδει`);
//! * Arabic: alef maqsura → yeh, teh marbuta → heh, tatweel removed, then
//!   a light prefix/suffix stemmer (`الإجازة`, `إجازات` → `اجاز`);
//! * Han, kana and Hangul runs, which do not separate words with spaces:
//!   every character and every overlapping character pair (`経費精算` →
//!   `経 費 精 算 経費 費精 精算`). A query searches a run by its pairs (a
//!   one-character run by the character).
//!
//! A word whose normal form is its lowercased self (plain English, code
//! identifiers) adds nothing. Changing any rule here changes indexed
//! content: bump [`LEXICAL_PROJECTION_VERSION`].

use std::collections::HashSet;

use unicode_normalization::char::is_combining_mark;
use unicode_normalization::UnicodeNormalization;

/// Identity of the projection rules, folded into the code and document
/// derivation versions (a change rebuilds every source).
pub const LEXICAL_PROJECTION_VERSION: &str = "lexnorm-1";

/// The script of one character, as far as normalization cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Script {
    Latin,
    Greek,
    Cyrillic,
    Arabic,
    Hebrew,
    Han,
    Kana,
    Hangul,
}

impl Script {
    /// ISO 15924 code.
    pub fn code(self) -> &'static str {
        match self {
            Self::Latin => "Latn",
            Self::Greek => "Grek",
            Self::Cyrillic => "Cyrl",
            Self::Arabic => "Arab",
            Self::Hebrew => "Hebr",
            Self::Han => "Hani",
            Self::Kana => "Kana",
            Self::Hangul => "Hang",
        }
    }

    /// Written without spaces between words: searched by characters and
    /// character pairs.
    pub fn is_cjk(self) -> bool {
        matches!(self, Self::Han | Self::Kana | Self::Hangul)
    }
}

/// The script of a letter, `None` for digits, punctuation, marks and
/// scripts normalization leaves alone.
pub fn script_of(c: char) -> Option<Script> {
    let u = c as u32;
    Some(match u {
        0x41..=0x5A | 0x61..=0x7A => Script::Latin,
        0xC0..=0x24F if u != 0xD7 && u != 0xF7 => Script::Latin,
        0x1E00..=0x1EFF | 0xFF21..=0xFF3A | 0xFF41..=0xFF5A => Script::Latin,
        0x370..=0x3FF | 0x1F00..=0x1FFF => Script::Greek,
        0x400..=0x52F => Script::Cyrillic,
        0x590..=0x5FF | 0xFB1D..=0xFB4F => Script::Hebrew,
        0x600..=0x6FF | 0x750..=0x77F | 0x8A0..=0x8FF | 0xFB50..=0xFDFF | 0xFE70..=0xFEFF => {
            Script::Arabic
        }
        0x3040..=0x30FF | 0x31F0..=0x31FF | 0xFF66..=0xFF9F => Script::Kana,
        0x3005 | 0x3007 | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF => Script::Han,
        0x20000..=0x2FA1F => Script::Han,
        0x1100..=0x11FF | 0x3130..=0x318F | 0xAC00..=0xD7AF => Script::Hangul,
        _ => return None,
    })
}

/// Characters of a word: letters, digits, `_` and combining marks.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || is_combining_mark(c)
}

/// The words of `text` (maximal runs of word characters).
pub fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !is_word_char(c))
        .filter(|w| !w.is_empty())
}

/// Lowercase, NFKC, no diacritics after letters of the scripts that
/// carry them as accents, `ß` → `ss`, `ς` → `σ`; Arabic letter variants
/// unified. No stemming.
pub fn fold(word: &str) -> String {
    let lower: String = word.nfkc().collect::<String>().to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut base: Option<Script> = None;
    for c in lower.nfd() {
        if is_combining_mark(c) {
            let strip = match base {
                Some(Script::Latin | Script::Greek | Script::Arabic | Script::Hebrew) => true,
                // ё and stress accents only.
                Some(Script::Cyrillic) => matches!(c, '\u{308}' | '\u{301}' | '\u{300}'),
                _ => false,
            };
            if !strip {
                out.push(c);
            }
            continue;
        }
        base = script_of(c);
        match c {
            'ß' => out.push_str("ss"),
            'ς' => out.push('σ'),
            // Tatweel is decoration.
            '\u{640}' => {}
            'ى' => out.push('ي'),
            'ة' => out.push('ه'),
            _ => out.push(c),
        }
    }
    out.nfc().collect()
}

/// Greek inflectional endings (folded), longest first.
const GREEK_SUFFIXES: &[&str] = &[
    "ουμαστε",
    "ομαστε",
    "ουνται",
    "ματων",
    "ματοσ",
    "ονται",
    "ουσαν",
    "ουται",
    "ματα",
    "σεων",
    "σεισ",
    "ουμε",
    "ουνε",
    "ουσα",
    "ουσε",
    "εται",
    "ομαι",
    "εσαι",
    "ειτε",
    "ηκαν",
    "ησαν",
    "σησ",
    "ουσ",
    "ουν",
    "εισ",
    "ετε",
    "ηκε",
    "ησε",
    "ασε",
    "ων",
    "ου",
    "οσ",
    "ον",
    "οι",
    "εσ",
    "ασ",
    "ησ",
    "ει",
    "μα",
    "ση",
    "αν",
    "α",
    "η",
    "ο",
    "ι",
    "ε",
    "ω",
    "υ",
];

/// Arabic prefixes (folded), longest first, removed when at least two
/// letters remain; `و` ("and") only when three remain.
const ARABIC_PREFIXES: &[&str] = &["وال", "بال", "كال", "فال", "لل", "ال"];

/// Arabic suffixes (folded), each removed once in this order when at
/// least two letters remain.
const ARABIC_SUFFIXES: &[&str] = &["ها", "ان", "ات", "ون", "ين", "يه", "ه", "ي"];

fn strip_suffix_min<'a>(w: &'a str, suffix: &str, min_left: usize) -> Option<&'a str> {
    let rest = w.strip_suffix(suffix)?;
    (rest.chars().count() >= min_left).then_some(rest)
}

fn stem_greek(w: &str) -> String {
    for s in GREEK_SUFFIXES {
        if let Some(rest) = strip_suffix_min(w, s, 3) {
            return rest.to_owned();
        }
    }
    w.to_owned()
}

fn stem_arabic(w: &str) -> String {
    let mut w = w.to_owned();
    if let Some(rest) = w.strip_prefix('و') {
        if rest.chars().count() >= 3 {
            w = rest.to_owned();
        }
    }
    for p in ARABIC_PREFIXES {
        if let Some(rest) = w.strip_prefix(p) {
            if rest.chars().count() >= 2 {
                w = rest.to_owned();
                break;
            }
        }
    }
    for s in ARABIC_SUFFIXES {
        if let Some(rest) = strip_suffix_min(&w, s, 2) {
            w = rest.to_owned();
        }
    }
    w
}

/// The normal form of a word without Han, kana or Hangul: [`fold`], then
/// the light stemmer of its script (Greek, Arabic).
pub fn normal_form(word: &str) -> String {
    let folded = fold(word);
    match dominant(&folded) {
        Some(Script::Greek) => stem_greek(&folded),
        Some(Script::Arabic) => stem_arabic(&folded),
        _ => folded,
    }
}

/// The script most letters of `w` are written in.
fn dominant(w: &str) -> Option<Script> {
    let mut counts: Vec<(Script, usize)> = Vec::new();
    for s in w.chars().filter_map(script_of) {
        match counts.iter_mut().find(|(k, _)| *k == s) {
            Some((_, n)) => *n += 1,
            None => counts.push((s, 1)),
        }
    }
    counts.into_iter().max_by_key(|(_, n)| *n).map(|(s, _)| s)
}

/// A word split into runs: `(true, run)` for Han/kana/Hangul characters,
/// `(false, run)` for the rest.
fn cjk_runs(word: &str) -> Vec<(bool, String)> {
    let mut out: Vec<(bool, String)> = Vec::new();
    for c in word.nfkc() {
        let cjk = script_of(c).is_some_and(Script::is_cjk);
        match out.last_mut() {
            Some((k, run)) if *k == cjk => run.push(c),
            _ => out.push((cjk, c.to_string())),
        }
    }
    out
}

fn chars_and_pairs(run: &str, out: &mut Vec<String>) {
    let chars: Vec<char> = run.chars().collect();
    out.extend(chars.iter().map(char::to_string));
    out.extend(chars.windows(2).map(|w| w.iter().collect::<String>()));
}

fn pairs_or_char(run: &str, out: &mut Vec<String>) {
    let chars: Vec<char> = run.chars().collect();
    if chars.len() == 1 {
        out.push(chars[0].to_string());
    } else {
        out.extend(chars.windows(2).map(|w| w.iter().collect::<String>()));
    }
}

/// The indexed forms of one word: empty when its normal form is its
/// lowercased self.
fn index_forms(word: &str, out: &mut Vec<String>) {
    if word
        .chars()
        .any(|c| script_of(c).is_some_and(Script::is_cjk))
    {
        for (cjk, run) in cjk_runs(word) {
            if cjk {
                chars_and_pairs(&run, out);
            } else {
                // The neighbouring digits and letters are their own words
                // here (the original tokenizer kept them with the run).
                out.push(normal_form(&run));
            }
        }
        return;
    }
    let n = normal_form(word);
    if n != word.to_lowercase() {
        out.push(n);
    }
}

/// The projection of `text`: the added forms of its words, space-separated
/// in text order (empty for plain English text).
pub fn projection(text: &str) -> String {
    let mut out = Vec::new();
    for w in words(text) {
        index_forms(w, &mut out);
    }
    out.join(" ")
}

/// What one query word is searched as in the projection: its normal form,
/// or the character pairs of its Han/kana/Hangul runs (a one-character run
/// by the character) with any neighbouring digits and letters. Empty when
/// the word adds no form (search it as written).
pub fn query_forms(word: &str) -> Vec<String> {
    let mut out = Vec::new();
    if word
        .chars()
        .any(|c| script_of(c).is_some_and(Script::is_cjk))
    {
        for (cjk, run) in cjk_runs(word) {
            if cjk {
                pairs_or_char(&run, &mut out);
            } else {
                out.push(normal_form(&run));
            }
        }
        return out;
    }
    let n = normal_form(word);
    if n != word.to_lowercase() {
        out.push(n);
    }
    out
}

/// Greek and Arabic function words (folded): never content terms.
const FUNCTION_WORDS: &[&str] = &[
    // Greek
    "ο",
    "η",
    "το",
    "οι",
    "τα",
    "του",
    "τησ",
    "των",
    "τον",
    "την",
    "τισ",
    "τουσ",
    "και",
    "να",
    "σε",
    "στο",
    "στη",
    "στην",
    "στον",
    "στα",
    "στισ",
    "στουσ",
    "με",
    "για",
    "απο",
    "που",
    "ποια",
    "ποιο",
    "ποιοσ",
    "ποιεσ",
    "ποιοι",
    "πωσ",
    "ποσο",
    "ποσα",
    "ποσεσ",
    "ποσοι",
    "ποτε",
    "ειναι",
    "ενα",
    "ενασ",
    "μια",
    "θα",
    "δεν",
    "μη",
    "μην",
    "αν",
    "κατα",
    "μετα",
    "προσ",
    "ωσ",
    "ειτε",
    "οταν",
    "αυτο",
    "αυτη",
    "αυτα",
    "εχει",
    "εχουν", // Arabic
    "في",
    "من",
    "على",
    "الي",
    "عن",
    "ما",
    "ماذا",
    "هل",
    "كم",
    "متي",
    "كيف",
    "او",
    "ان",
    "هي",
    "هو",
    "التي",
    "الذي",
    "هذا",
    "هذه",
    "مع",
    "بعد",
    "قبل",
];

/// Whether `word` is a Greek or Arabic function word.
pub fn is_function_word(word: &str) -> bool {
    let f = fold(word);
    FUNCTION_WORDS.contains(&f.as_str())
}

/// Term matching on normalized forms, for checks that read text rather
/// than an index (grounding support, term-coverage ordering).
#[derive(Debug, Clone)]
pub struct TermMatcher {
    lower: String,
    forms: HashSet<String>,
}

impl TermMatcher {
    pub fn new(text: &str) -> Self {
        let mut forms = HashSet::new();
        for w in words(text) {
            let mut f = Vec::new();
            index_forms(w, &mut f);
            if f.is_empty() {
                forms.insert(w.to_lowercase());
            }
            forms.extend(f);
        }
        Self {
            lower: text.to_lowercase(),
            forms,
        }
    }

    /// Whether the text contains `term` as written (case-insensitively),
    /// or its normal form; a Han/kana/Hangul term when at least half of
    /// its character pairs occur.
    pub fn contains(&self, term: &str) -> bool {
        let lower = term.to_lowercase();
        if self.lower.contains(&lower) {
            return true;
        }
        let forms = query_forms(term);
        match forms.len() {
            0 => false,
            1 => self.forms.contains(&forms[0]),
            n => forms.iter().filter(|f| self.forms.contains(*f)).count() * 2 >= n,
        }
    }
}

/// The script most of `text`'s letters are written in, with its share of
/// the letters; `None` without letters.
pub fn dominant_script(text: &str) -> Option<(Script, f64)> {
    let mut counts: std::collections::BTreeMap<Script, usize> = Default::default();
    let mut total = 0usize;
    for s in text.chars().filter_map(script_of) {
        *counts.entry(s).or_default() += 1;
        total += 1;
    }
    let (s, n) = counts.into_iter().max_by_key(|(_, n)| *n)?;
    Some((s, n as f64 / total as f64))
}

/// Function words of the Latin-script languages [`detect_language`]
/// tells apart.
const LATIN_PROFILES: &[(&str, &[&str])] = &[
    (
        "en",
        &[
            "the", "and", "of", "to", "is", "in", "that", "for", "are", "with", "on", "be", "this",
            "by", "or", "must", "within", "after",
        ],
    ),
    (
        "de",
        &[
            "der",
            "die",
            "das",
            "und",
            "ist",
            "nicht",
            "mit",
            "von",
            "zu",
            "den",
            "ein",
            "eine",
            "für",
            "auf",
            "werden",
            "nach",
            "innerhalb",
            "sind",
        ],
    ),
    (
        "fr",
        &[
            "le", "la", "les", "et", "est", "des", "un", "une", "du", "pour", "dans", "que",
            "sont", "avec", "pas", "au", "doit", "après",
        ],
    ),
    (
        "es",
        &[
            "el", "los", "las", "y", "es", "del", "un", "una", "para", "con", "por", "que", "en",
            "se", "son", "debe",
        ],
    ),
    (
        "it",
        &[
            "il", "lo", "gli", "e", "è", "di", "che", "per", "con", "una", "sono", "della", "deve",
        ],
    ),
    (
        "pt",
        &[
            "o", "os", "as", "e", "é", "do", "da", "dos", "um", "uma", "para", "com", "não", "deve",
        ],
    ),
    (
        "nl",
        &[
            "de", "het", "een", "en", "is", "van", "niet", "met", "voor", "zijn", "op", "moet",
        ],
    ),
];

/// Fewest letters a language is detected from.
const MIN_LETTERS: usize = 20;

/// The language of `text` (ISO 639-1) where it is reliable, else `None`:
/// by script where one language dominates it (Greek `el`, kana `ja`, Han
/// without kana `zh`, Hangul `ko`, Hebrew `he`, Arabic script `ar` or `fa`
/// with Persian letters), by function words for Latin script (English,
/// German, French, Spanish, Italian, Portuguese, Dutch) when one language
/// clearly leads. Cyrillic is not told apart.
pub fn detect_language(text: &str) -> Option<&'static str> {
    let letters: Vec<Script> = text.chars().filter_map(script_of).collect();
    if letters.len() < MIN_LETTERS {
        return None;
    }
    let (script, share) = dominant_script(text)?;
    let kana = letters.contains(&Script::Kana);
    if share < 0.6 && !(kana && matches!(script, Script::Han | Script::Kana)) {
        return None;
    }
    match script {
        Script::Greek => Some("el"),
        Script::Kana => Some("ja"),
        Script::Han if kana => Some("ja"),
        Script::Han => Some("zh"),
        Script::Hangul => Some("ko"),
        Script::Hebrew => Some("he"),
        Script::Arabic if text.chars().any(|c| matches!(c, 'پ' | 'چ' | 'ژ' | 'گ')) => {
            Some("fa")
        }
        Script::Arabic => Some("ar"),
        Script::Cyrillic => None,
        Script::Latin => {
            let lower: Vec<String> = text
                .split(|c: char| !c.is_alphabetic())
                .filter(|w| !w.is_empty())
                .map(str::to_lowercase)
                .collect();
            let mut scores: Vec<(&str, usize)> = LATIN_PROFILES
                .iter()
                .map(|(lang, fw)| {
                    (
                        *lang,
                        lower.iter().filter(|w| fw.contains(&w.as_str())).count(),
                    )
                })
                .collect();
            scores.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
            let (best, n) = scores[0];
            let second = scores[1].1;
            // Clearly ahead: at least three function words and half again
            // as many as the runner-up.
            (n >= 3 && n * 2 >= second * 3).then_some(best)
        }
    }
}

/// The script code of `text` where one script holds most of its letters.
pub fn detect_script(text: &str) -> Option<&'static str> {
    let letters = text.chars().filter_map(script_of).count();
    if letters < MIN_LETTERS {
        return None;
    }
    let (script, share) = dominant_script(text)?;
    let kana = text.chars().any(|c| script_of(c) == Some(Script::Kana));
    if kana && matches!(script, Script::Han | Script::Kana) {
        // Japanese: Han and kana together.
        return Some("Jpan");
    }
    (share >= 0.6).then(|| script.code())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_and_identifiers_add_nothing() {
        assert_eq!(projection("Refunds above 500 euros need approval"), "");
        assert_eq!(projection("ProjectStore::open(layout)"), "");
        assert!(query_forms("refund").is_empty());
    }

    #[test]
    fn accents_case_and_compatibility_forms_fold() {
        assert_eq!(fold("Café"), "cafe");
        assert_eq!(fold("Straße"), "strasse");
        assert_eq!(fold("ＡＢＣ１２"), "abc12");
        assert_eq!(fold("ﬁle"), "file");
        assert_eq!(projection("Résumé naïve"), "resume naive");
        // Kana keep their marks: が is not か.
        assert_eq!(fold("が"), "が");
        // Cyrillic: ё folds, й does not.
        assert_eq!(fold("Ёлка"), "елка");
        assert_eq!(fold("Йод"), "йод");
    }

    #[test]
    fn greek_forms_share_a_stem() {
        for w in ["άδεια", "άδειας", "άδειες", "ΑΔΕΙΑ"] {
            assert_eq!(normal_form(w), "αδει", "{w}");
        }
        for w in ["ημέρα", "ημέρες", "ημερών", "ημέρας"] {
            assert_eq!(normal_form(w), "ημερ", "{w}");
        }
        for w in ["κωδικός", "κωδικού", "κωδικοί", "κωδικών"] {
            assert_eq!(normal_form(w), "κωδικ", "{w}");
        }
        assert_eq!(normal_form("αιτήσεις"), normal_form("αίτηση"));
        // Final sigma.
        assert_eq!(fold("ΟΔΟΣ"), "οδοσ");
        // Short words keep their ending.
        assert_eq!(normal_form("και"), "και");
        assert!(is_function_word("Πόσες"));
        assert!(!is_function_word("άδεια"));
    }

    #[test]
    fn arabic_letter_variants_and_affixes() {
        assert_eq!(normal_form("الإجازة"), "اجاز");
        assert_eq!(normal_form("إجازات"), "اجاز");
        assert_eq!(normal_form("إجازة"), "اجاز");
        // Harakat and tatweel.
        assert_eq!(fold("كِتَـــاب"), "كتاب");
        assert!(is_function_word("في"));
    }

    #[test]
    fn cjk_runs_index_characters_and_pairs_and_query_by_pairs() {
        assert_eq!(projection("経費精算"), "経 費 精 算 経費 費精 精算");
        assert_eq!(query_forms("経費精算"), ["経費", "費精", "精算"]);
        assert_eq!(query_forms("日"), ["日"]);
        // Digits next to a run are their own word.
        assert_eq!(query_forms("30日以内"), ["30", "日以", "以内"]);
        assert_eq!(projection("30日"), "30 日");
        // Full-width digits fold.
        assert_eq!(query_forms("３０日"), ["30", "日"]);
    }

    #[test]
    fn term_matcher_finds_inflected_and_unsegmented_terms() {
        let greek = TermMatcher::new("Οι υπάλληλοι δικαιούνται 25 ημέρες άδειας.");
        assert!(greek.contains("άδεια"));
        assert!(greek.contains("ημερών"));
        assert!(!greek.contains("μισθός"));
        let ja = TermMatcher::new("経費精算は30日以内に提出してください。");
        assert!(ja.contains("経費精算の期限"), "half the pairs occur");
        assert!(!ja.contains("出張旅費"));
        assert!(TermMatcher::new("Refunds need approval").contains("approval"));
    }

    #[test]
    fn languages_are_detected_only_where_reliable() {
        assert_eq!(
            detect_language("Οι υπάλληλοι δικαιούνται είκοσι πέντε ημέρες άδειας κάθε χρόνο."),
            Some("el")
        );
        assert_eq!(
            detect_language("経費精算は三十日以内に提出してください。領収書を添付します。"),
            Some("ja")
        );
        assert_eq!(
            detect_language("员工每年享有二十五天带薪年假，需要提前两周申请并经过经理批准。"),
            Some("zh")
        );
        assert_eq!(
            detect_language(
                "يحق للموظفين الحصول على إجازة سنوية مدفوعة الأجر لمدة خمسة وعشرين يوما"
            ),
            Some("ar")
        );
        assert_eq!(
            detect_language("All expense claims must be submitted within thirty days of the trip."),
            Some("en")
        );
        assert_eq!(
            detect_language("Die Reisekosten müssen innerhalb von dreißig Tagen nach der Reise eingereicht werden."),
            Some("de")
        );
        assert_eq!(
            detect_language(
                "Les notes de frais doivent être soumises dans les trente jours après le voyage."
            ),
            Some("fr")
        );
        // Too short, or no clear winner.
        assert_eq!(detect_language("OK"), None);
        assert_eq!(
            detect_language("ProjectStore open layout build source"),
            None
        );
        assert_eq!(
            detect_script("経費精算は三十日以内に提出してください。領収書を添付します。"),
            Some("Jpan")
        );
        assert_eq!(
            detect_script("Οι υπάλληλοι δικαιούνται είκοσι πέντε ημέρες άδειας."),
            Some("Grek")
        );
    }
}
