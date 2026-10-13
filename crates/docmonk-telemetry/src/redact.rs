//! Credential/URL redaction for anything printed or logged.

use std::sync::OnceLock;

use regex::Regex;

use crate::urlsplit::{urlsplit, urlunsplit};

/// Env vars server credentials are read from at call time, per engine.
pub const CREDENTIAL_ENV_VARS: &[(&str, [&str; 3])] = &[
    (
        "opensearch",
        [
            "RAGMONK_OPENSEARCH_USERNAME",
            "RAGMONK_OPENSEARCH_PASSWORD",
            "RAGMONK_OPENSEARCH_API_KEY",
        ],
    ),
    (
        "elasticsearch",
        [
            "RAGMONK_ELASTICSEARCH_USERNAME",
            "RAGMONK_ELASTICSEARCH_PASSWORD",
            "RAGMONK_ELASTICSEARCH_API_KEY",
        ],
    ),
];

pub fn credential_env_vars(engine: &str) -> Option<[&'static str; 3]> {
    CREDENTIAL_ENV_VARS
        .iter()
        .find(|(e, _)| *e == engine)
        .map(|(_, v)| *v)
}

fn url_userinfo_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"([a-zA-Z][a-zA-Z0-9+.\-]*://)[^\s/@]+@").expect("static regex"))
}

/// Scrubs every URL user-info, bare `user:password@host` and the value of
/// any configured credential env var (>= 4 chars) from `text`.
pub fn redact_urls_in_text(text: &str) -> String {
    redact_urls_in_text_with(text, &|k| std::env::var(k).ok())
}

pub fn redact_urls_in_text_with(text: &str, env: &dyn Fn(&str) -> Option<String>) -> String {
    if text.is_empty() {
        return String::new();
    }
    let text = url_userinfo_re().replace_all(text, "$1");
    let mut text = strip_bare_userinfo(&text);
    for (_, names) in CREDENTIAL_ENV_VARS {
        for name in names {
            if let Some(value) = env(name) {
                if value.chars().count() >= 4 && text.contains(&value) {
                    text = text.replace(&value, "***");
                }
            }
        }
    }
    text
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Port of `(?<![\w/@])[^\s/@:]+:[^\s/@]+@(?=[\w.\-\[])` substituted with
/// "" (Rust's regex crate has no look-around). The pattern is
/// deterministic per start position: each run is maximal because its
/// character class excludes the delimiter that must follow it.
fn strip_bare_userinfo(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let user_char = |c: char| !c.is_whitespace() && c != '/' && c != '@' && c != ':';
    let pass_char = |c: char| !c.is_whitespace() && c != '/' && c != '@';
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let prev_ok =
            i == 0 || !(is_word(chars[i - 1]) || chars[i - 1] == '/' || chars[i - 1] == '@');
        if prev_ok {
            if let Some(end) = match_userinfo_at(&chars, i, &user_char, &pass_char) {
                i = end;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn match_userinfo_at(
    chars: &[char],
    start: usize,
    user_char: &dyn Fn(char) -> bool,
    pass_char: &dyn Fn(char) -> bool,
) -> Option<usize> {
    let mut j = start;
    while j < chars.len() && user_char(chars[j]) {
        j += 1;
    }
    if j == start || chars.get(j) != Some(&':') {
        return None;
    }
    j += 1;
    let pass_start = j;
    while j < chars.len() && pass_char(chars[j]) {
        j += 1;
    }
    if j == pass_start || chars.get(j) != Some(&'@') {
        return None;
    }
    j += 1;
    let next = *chars.get(j)?;
    (is_word(next) || next == '.' || next == '-' || next == '[').then_some(j)
}

/// Strips embedded user-info from one URL. Never fails.
pub fn redact_url(url: &str) -> String {
    if url.is_empty() {
        return String::new();
    }
    let Ok(mut parts) = urlsplit(url) else {
        return match url.rsplit_once('@') {
            Some((_, host)) => host.to_owned(),
            None => url.to_owned(),
        };
    };
    if !parts.netloc.contains('@') {
        return url.to_owned();
    }
    parts.netloc = parts
        .netloc
        .rsplit_once('@')
        .map(|(_, h)| h.to_owned())
        .unwrap_or_default();
    urlunsplit(&parts)
}

/// True when the URL's authority carries `user[:password]@`.
pub fn url_has_userinfo(url: &str) -> bool {
    if url.is_empty() || !url.contains('@') {
        return false;
    }
    match urlsplit(url) {
        Err(_) => true,
        Ok(parts) if !parts.netloc.is_empty() => parts.netloc.contains('@'),
        Ok(_) => url.split('/').next().unwrap_or("").contains('@'),
    }
}
