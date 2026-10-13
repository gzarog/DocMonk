//! Standard `urlsplit`/`urlunsplit` URL splitting, restricted to what the
//! redaction helpers need.

const SCHEME_CHARS: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789+-.";

const USES_NETLOC: &[&str] = &[
    "",
    "ftp",
    "http",
    "gopher",
    "nntp",
    "telnet",
    "imap",
    "wais",
    "file",
    "mms",
    "https",
    "shttp",
    "snews",
    "prospero",
    "rtsp",
    "rtsps",
    "rtspu",
    "rsync",
    "svn",
    "svn+ssh",
    "sftp",
    "nfs",
    "git",
    "git+ssh",
    "ws",
    "wss",
    "itms-services",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitResult {
    pub scheme: String,
    pub netloc: String,
    pub path: String,
    pub query: String,
    pub fragment: String,
}

/// Rejects malformed bracketed hosts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidUrl;

pub fn urlsplit(url: &str) -> Result<SplitResult, InvalidUrl> {
    // WHATWG: strip leading C0 control/space, drop tab/CR/LF anywhere.
    let url = url.trim_start_matches(|c: char| c <= ' ');
    let mut url: String = url
        .chars()
        .filter(|c| !matches!(c, '\t' | '\r' | '\n'))
        .collect();
    let mut scheme = String::new();
    if let Some(i) = url.find(':') {
        let first = url.chars().next();
        if i > 0
            && first.is_some_and(|c| c.is_ascii_alphabetic())
            && url[..i].chars().all(|c| SCHEME_CHARS.contains(c))
        {
            scheme = url[..i].to_ascii_lowercase();
            url = url[i + 1..].to_owned();
        }
    }
    let mut netloc = String::new();
    if url.starts_with("//") {
        let rest = &url[2..];
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        netloc = rest[..end].to_owned();
        url = rest[end..].to_owned();
        let open = netloc.contains('[');
        let close = netloc.contains(']');
        if open != close {
            return Err(InvalidUrl);
        }
        if open && close {
            let host = netloc
                .split_once('[')
                .map(|(_, r)| r)
                .unwrap_or("")
                .split_once(']')
                .map(|(h, _)| h)
                .unwrap_or("");
            check_bracketed_host(host)?;
        }
    }
    let mut fragment = String::new();
    if let Some((before, after)) = url.split_once('#') {
        fragment = after.to_owned();
        url = before.to_owned();
    }
    let mut query = String::new();
    if let Some((before, after)) = url.split_once('?') {
        query = after.to_owned();
        url = before.to_owned();
    }
    Ok(SplitResult {
        scheme,
        netloc,
        path: url,
        query,
        fragment,
    })
}

fn check_bracketed_host(host: &str) -> Result<(), InvalidUrl> {
    if let Some(rest) = host.strip_prefix('v') {
        // IPvFuture: v<hex>.<unreserved / sub-delims / ':'>+
        let ok = rest.split_once('.').is_some_and(|(hex, tail)| {
            !hex.is_empty()
                && hex.chars().all(|c| c.is_ascii_hexdigit())
                && !tail.is_empty()
                && tail
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-._~!$&'()*+,;=:".contains(c))
        });
        return if ok { Ok(()) } else { Err(InvalidUrl) };
    }
    let addr = host.split('%').next().unwrap_or("");
    addr.parse::<std::net::Ipv6Addr>()
        .map(|_| ())
        .map_err(|_| InvalidUrl)
}

pub fn urlunsplit(parts: &SplitResult) -> String {
    let SplitResult {
        scheme,
        netloc,
        path,
        query,
        fragment,
    } = parts;
    let mut url = path.clone();
    if !netloc.is_empty() {
        if !url.is_empty() && !url.starts_with('/') {
            url.insert(0, '/');
        }
        url = format!("//{netloc}{url}");
    } else if url.starts_with("//")
        || (!scheme.is_empty()
            && USES_NETLOC.contains(&scheme.as_str())
            && (url.is_empty() || url.starts_with('/')))
    {
        url = format!("//{url}");
    }
    if !scheme.is_empty() {
        url = format!("{scheme}:{url}");
    }
    if !query.is_empty() {
        url = format!("{url}?{query}");
    }
    if !fragment.is_empty() {
        url = format!("{url}#{fragment}");
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_url_components() {
        let s = urlsplit("HTTPS://u:p@h:9200/x?q=1#f").unwrap();
        assert_eq!(s.scheme, "https");
        assert_eq!(s.netloc, "u:p@h:9200");
        assert_eq!(
            (s.path.as_str(), s.query.as_str(), s.fragment.as_str()),
            ("/x", "q=1", "f")
        );
        let s = urlsplit("user:pw@host:9200").unwrap();
        assert_eq!((s.scheme.as_str(), s.netloc.as_str()), ("user", ""));
        assert!(urlsplit("http://[::1").is_err());
        assert!(urlsplit("http://[zz]/").is_err());
        assert!(urlsplit("http://[::1]:9200").is_ok());
    }
}
