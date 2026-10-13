//! Exact body-token counting and token-budget splitting.

use crate::tokenizer::model_tokenizer;

/// Whitespace test (Unicode White_Space plus the C0 separators
/// U+001C..U+001F).
pub fn py_isspace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

pub fn py_strip(s: &str) -> &str {
    s.trim_matches(py_isspace)
}

/// `str.split()` with no separator.
pub fn py_split_whitespace(s: &str) -> Vec<&str> {
    s.split(py_isspace).filter(|w| !w.is_empty()).collect()
}

/// Exact body-token count (no special tokens).
pub fn count_tokens(text: &str) -> i64 {
    if text.is_empty() {
        return 0;
    }
    model_tokenizer().expect("tokenizer").count(text, false)
}

/// `re.split(r"(?<=[.!?])\s+", text.strip())`, dropping empty parts.
pub fn split_sentences(text: &str) -> Vec<String> {
    let stripped = py_strip(text);
    if stripped.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut start = 0;
    let mut prev: Option<char> = None;
    let mut iter = stripped.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        if py_isspace(c) && matches!(prev, Some('.' | '!' | '?')) {
            let mut end = i + c.len_utf8();
            while let Some(&(j, d)) = iter.peek() {
                if !py_isspace(d) {
                    break;
                }
                end = j + d.len_utf8();
                iter.next();
            }
            out.push(stripped[start..i].to_owned());
            start = end;
            prev = None;
            continue;
        }
        prev = Some(c);
    }
    out.push(stripped[start..].to_owned());
    out.retain(|s| !s.is_empty());
    out
}

/// Pieces each within `max_tokens` body tokens: sentences, then words,
/// then sub-word offsets for a single oversized word; re-packed.
pub fn split_by_token_budget(text: &str, max_tokens: i64) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let tok = model_tokenizer().expect("tokenizer");
    if tok.count(text, false) <= max_tokens {
        return vec![text.to_owned()];
    }
    let mut pieces: Vec<String> = Vec::new();
    for sentence in split_sentences(text) {
        if tok.count(&sentence, false) <= max_tokens {
            pieces.push(sentence);
            continue;
        }
        let mut buf: Vec<&str> = Vec::new();
        let mut buf_tokens = 0;
        for word in py_split_whitespace(&sentence) {
            let wt = tok.count(word, false);
            if wt > max_tokens {
                if !buf.is_empty() {
                    pieces.push(buf.join(" "));
                    buf.clear();
                    buf_tokens = 0;
                }
                pieces.extend(tok.split(word, max_tokens));
                continue;
            }
            if !buf.is_empty() && buf_tokens + wt > max_tokens {
                pieces.push(buf.join(" "));
                buf.clear();
                buf_tokens = 0;
            }
            buf.push(word);
            buf_tokens += wt;
        }
        if !buf.is_empty() {
            pieces.push(buf.join(" "));
        }
    }
    repack(&pieces, max_tokens)
}

fn repack(pieces: &[String], max_tokens: i64) -> Vec<String> {
    let mut packed = Vec::new();
    let mut buf: Vec<&str> = Vec::new();
    let mut buf_tokens = 0;
    for piece in pieces {
        let pt = count_tokens(piece);
        if !buf.is_empty() && buf_tokens + pt > max_tokens {
            packed.push(buf.join(" "));
            buf.clear();
            buf_tokens = 0;
        }
        buf.push(piece);
        buf_tokens += pt;
    }
    if !buf.is_empty() {
        packed.push(buf.join(" "));
    }
    packed
}
