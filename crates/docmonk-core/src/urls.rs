//! URL path anchors and literals: one normalization shared by query-time
//! anchor extraction and index-time literal extraction, so an exact match
//! means the same thing on every backend.

/// `target_symbol` prefix of an indexed URL string literal
/// (`url_literal:<key>`).
pub const URL_LITERAL_PREFIX: &str = "url_literal:";
/// `target_symbol` prefix of a statically composed framework route
/// (`route:<key>`).
pub const ROUTE_PREFIX: &str = "route:";

/// Extensions that make a dotted token a file name, not a symbol or an
/// endpoint segment.
pub const FILE_EXTENSIONS: &[&str] = &[
    "md", "txt", "pdf", "eml", "docx", "doc", "odt", "pptx", "xlsx", "csv", "html", "htm", "json",
    "yaml", "yml", "toml", "xml", "cs", "py", "rs", "ts", "tsx", "js", "jsx", "go", "java", "kt",
    "rb", "php", "sql", "sh", "png", "jpg",
];

/// The path part of a URL or route: scheme/host, query and fragment
/// dropped, a leading `~` removed, `\` read as `/`, duplicate slashes
/// collapsed, with exactly one leading `/` (`""` when nothing remains).
/// Case and parameters are kept as written.
pub fn url_path(raw: &str) -> String {
    let mut s = raw.trim();
    if let Some(i) = s.find("://") {
        let rest = &s[i + 3..];
        s = rest.find('/').map_or("", |j| &rest[j..]);
    }
    let s = s.split(['?', '#']).next().unwrap_or("");
    let s = s.trim_start_matches('~').replace('\\', "/");
    let mut out = String::with_capacity(s.len() + 1);
    for seg in s.split('/').filter(|p| !p.is_empty()) {
        out.push('/');
        out.push_str(seg);
    }
    out
}

/// The exact-match key of a URL path or route template:
/// [`url_path`] without its leading `/`, route parameters (`{id}`,
/// `{id:int}`, Flask's `<int:id>`) written `{}`, ASCII lower-cased (ASP.NET routing and most
/// HTTP routers match paths case-insensitively). Percent escapes are kept
/// as written (lower-cased hex). Segment boundaries are preserved, so
/// `/a/image` and `/a/image2` never share a key. `None` when no path
/// remains.
pub fn url_match_key(raw: &str) -> Option<String> {
    let path = url_path(raw);
    let mut out = String::with_capacity(path.len());
    let mut depth = 0usize;
    for c in path.trim_start_matches('/').chars() {
        match c {
            '{' | '<' => {
                if depth == 0 {
                    out.push('{');
                }
                depth += 1;
            }
            '}' | '>' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    out.push('}');
                }
            }
            _ if depth > 0 => {}
            c => out.push(c.to_ascii_lowercase()),
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The keys a query anchor also matches as a route template: the key with
/// one segment at a time written `{}` (`a/b/c` -> `{}/b/c`, `a/{}/c`,
/// `a/b/{}`), at most `max` of them, last segments first.
pub fn template_variants(key: &str, max: usize) -> Vec<String> {
    let segs: Vec<&str> = key.split('/').collect();
    let mut out = Vec::new();
    for i in (0..segs.len()).rev() {
        if out.len() >= max {
            break;
        }
        if segs[i] == "{}" {
            continue;
        }
        let mut v = segs.clone();
        v[i] = "{}";
        out.push(v.join("/"));
    }
    out
}

/// Characters a URL path token may contain.
pub fn is_url_char(c: char) -> bool {
    c.is_alphanumeric() || "/-._~%{}:@!$&'()*+,;=".contains(c)
}

fn versioned(segment: &str) -> bool {
    segment.len() >= 2
        && segment.starts_with('v')
        && segment[1..].chars().all(|c| c.is_ascii_digit())
}

/// Whether a slash-delimited token written in prose is an API endpoint
/// path rather than plain English ("route/client/API", "and/or"): it
/// starts with `/` or `~/`, is an absolute http(s) URL, carries a
/// `{param}` with at least two segments, or its first segment is `api` or
/// a version (`v<N>`) with at least two segments.
pub fn looks_like_endpoint(token: &str) -> bool {
    if token.starts_with("http://") || token.starts_with("https://") {
        return true;
    }
    let lower = token.to_ascii_lowercase();
    let trimmed = lower.trim_start_matches('~').trim_start_matches('/');
    let first = trimmed.split('/').next().unwrap_or("");
    let segments = lower.split('/').filter(|s| !s.is_empty()).count();
    let rooted = (token.starts_with('/') && !token.starts_with("//")) || token.starts_with("~/");
    (rooted && segments >= 1)
        || (token.contains('{') && token.contains('}') && segments >= 2)
        || ((first == "api" || versioned(first)) && segments >= 2)
}

/// Whether a code string literal's content is worth indexing as a URL
/// literal: no whitespace, only URL characters, at least one `/`, not a
/// relative file path (`./x`, `../x`) or a file name with an extension in
/// `file_extensions`, and either [`looks_like_endpoint`] or at least three
/// path segments (`shop/content/groups` passed to a client with a base
/// address). MIME types (`image/png`) and other two-segment strings are
/// left out.
pub fn is_url_literal(text: &str, file_extensions: &[&str]) -> bool {
    let t = text.trim();
    if t.is_empty()
        || t.len() > 512
        || !t.contains('/')
        || t.starts_with("./")
        || t.starts_with("../")
        || t.chars().any(char::is_whitespace)
    {
        return false;
    }
    let path = url_path(t);
    if path.is_empty() || !path.chars().all(|c| is_url_char(c) || c == '?' || c == '#') {
        return false;
    }
    if !t.contains("://") {
        let last = path.rsplit('/').next().unwrap_or("");
        if let Some((stem, ext)) = last.rsplit_once('.') {
            if !stem.is_empty() && file_extensions.contains(&ext.to_ascii_lowercase().as_str()) {
                return false;
            }
        }
    }
    let segments = path.split('/').filter(|s| !s.is_empty()).count();
    looks_like_endpoint(t) || segments >= 3
}

#[cfg(test)]
mod tests {
    use super::*;

    // clean-slate-audit: allow-start (API URL paths with a version segment are user data)
    #[test]
    fn keys_keep_segment_boundaries() {
        assert_eq!(url_path("https://h.example.com//SB/v1/x?q=1#f"), "/SB/v1/x");
        assert_eq!(
            url_match_key("~/sb/v1/{id:int}/").as_deref(),
            Some("sb/v1/{}")
        );
        assert_ne!(url_match_key("/a/image2"), url_match_key("/a/image"));
        assert_eq!(
            url_match_key("/kennels/<int:id>").as_deref(),
            Some("kennels/{}")
        );
        assert_eq!(url_match_key("/"), None);
        assert_eq!(
            template_variants("a/b/c", 8),
            ["a/b/{}", "a/{}/c", "{}/b/c"]
        );
        assert_eq!(template_variants("a/{}/c", 1), ["a/{}/{}"]);
    }

    #[test]
    fn literals_and_endpoints() {
        let ext = ["cs", "json"];
        assert!(is_url_literal("/sb/v1/content/groups/image", &ext));
        assert!(is_url_literal("sb/v1/content/groups/image", &ext));
        assert!(is_url_literal("https://h/x", &ext));
        assert!(is_url_literal("api/orders", &ext));
        assert!(!is_url_literal("image/png", &ext));
        assert!(!is_url_literal("src/Billing/Ledger.cs", &ext));
        assert!(!is_url_literal("hello world/x/y", &ext));
        assert!(!is_url_literal("../a/b/c", &ext));
        assert!(looks_like_endpoint("/health"));
        assert!(!looks_like_endpoint("route/client/API"));
        assert!(!looks_like_endpoint("//comment"));
    }
    // clean-slate-audit: allow-end
}
