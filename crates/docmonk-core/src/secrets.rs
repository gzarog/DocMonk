//! Content redaction of credentials in indexed text (`docmonk.secrets`).
//!
//! Every text DocMonk stores or embeds from a source file passes through
//! [`redact`] first (when `indexing.redact_secrets` is on), so a credential
//! written into a config file, a snippet in a Markdown page or a key pasted
//! into an email is never searchable, embedded, cited or returned through
//! MCP. Only the secret value is replaced, with `[REDACTED:<kind>]` on the
//! same line: line numbers, spans and citations stay valid, and the key name
//! (`DB_PASSWORD=`) stays searchable.
//!
//! The rules are deliberately conservative about code: an assignment is only
//! redacted when its value is a literal (quoted, or a bare config-style
//! value), never an expression such as `password = read_password()`.

use std::borrow::Cow;
use std::sync::OnceLock;

use regex::{Captures, Regex};

/// Identity of the rule set. Folded into the parser and chunker versions,
/// so changing a rule (or toggling redaction) rebuilds affected sources.
pub const REDACTION_VERSION: &str = "redact.1";

const PRIVATE_KEY: &str = "private_key";

struct Rule {
    kind: &'static str,
    re: Regex,
    /// The capture group holding the secret value (0: the whole match).
    group: usize,
}

fn rules() -> &'static [Rule] {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(|| {
        let r = |kind, pattern: &str, group| Rule {
            kind,
            re: Regex::new(pattern).expect("static regex"),
            group,
        };
        vec![
            // Well-known token formats first: they are unambiguous anywhere.
            r("aws_access_key", r"\b(?:AKIA|ASIA|ABIA|ACCA)[0-9A-Z]{16}\b", 0),
            r("github_token", r"\bgh[pousr]_[A-Za-z0-9]{36,255}\b", 0),
            r("github_token", r"\bgithub_pat_[A-Za-z0-9_]{22,255}\b", 0),
            r("gitlab_token", r"\bglpat-[A-Za-z0-9_\-]{20,}\b", 0),
            r("slack_token", r"\bxox[abposr]-[A-Za-z0-9\-]{10,}\b", 0),
            r("google_api_key", r"\bAIza[0-9A-Za-z_\-]{35}\b", 0),
            r("stripe_key", r"\b[sr]k_live_[0-9A-Za-z]{20,}\b", 0),
            r(
                "jwt",
                r"\beyJ[A-Za-z0-9_\-]{8,}\.eyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}",
                0,
            ),
            // scheme://user:password@host: only the password.
            r(
                "url_credentials",
                r"\b[a-zA-Z][a-zA-Z0-9+.\-]*://[^\s/:@]+:([^\s/@\[]+)@",
                1,
            ),
            // ADO.NET / ODBC connection strings: Password=...; Pwd=...
            r(
                "connection_string",
                r#"(?i)[;"']\s*(?:password|pwd)\s*=\s*([^;"'\s\[][^;"']*?)\s*(?:;|"|'|$)"#,
                1,
            ),
            r(
                "connection_string",
                r#"(?i)^\s*(?:password|pwd)\s*=\s*([^;"'\s\[][^;"']*?)\s*;"#,
                1,
            ),
            // key = "literal" / key: 'literal' / "key": "literal" / key => "literal"
            r(
                "assignment",
                r#"(?i)[\w.\-]*(?:password|passwd|secret|api[_\-]?key|apikey|access[_\-]?key|auth[_\-]?token|access[_\-]?token|refresh[_\-]?token|client[_\-]?secret|private[_\-]?key|token|credential)[\w.\-]*["']?\s*(?::|=|=>|:=)\s*(?:["'])([^"'\s\[][^"']{3,})["']"#,
                1,
            ),
            // Environment-style KEY=value anywhere in a line (shell snippets,
            // prose, log lines): an upper-case key, no spaces around `=`.
            r(
                "assignment",
                r#"\b[A-Z0-9_]*(?:PASSWORD|PASSWD|SECRET|API_?KEY|ACCESS_KEY|TOKEN|CREDENTIAL)[A-Z0-9_]*=([^\s"'#;,()\[\]\$\{<|>&*][^\s"'#;,()\[\]]{3,})"#,
                1,
            ),
            // Config-style bare values on their own line (.env, YAML, INI,
            // .properties): KEY=value, key: value, export KEY=value.
            r(
                "assignment",
                r#"(?i)^\s*(?:export\s+)?[\w.\-]*(?:password|passwd|secret|api[_\-]?key|apikey|access[_\-]?key|auth[_\-]?token|access[_\-]?token|refresh[_\-]?token|client[_\-]?secret|private[_\-]?key|token|credential)[\w.\-]*\s*[:=]\s*([^\s"'#;,\[\$\{\(<|>&*][^\s#;,()\[\]"']{3,})\s*$"#,
                1,
            ),
        ]
    })
}

/// A bare value that is plainly not a secret: a boolean or null, or a
/// reference to another field (`self.token`, `this.password`).
fn is_benign_bare(value: &str) -> bool {
    let v = value.to_ascii_lowercase();
    matches!(
        v.as_str(),
        "true" | "false" | "null" | "none" | "nil" | "yes" | "no" | "required" | "optional"
    ) || v.starts_with("self.")
        || v.starts_with("this.")
        || v.starts_with("process.env")
        || v.starts_with("os.environ")
        || v.starts_with("env.")
}

fn redact_line(line: &str) -> Cow<'_, str> {
    let mut out = Cow::Borrowed(line);
    for rule in rules() {
        if !rule.re.is_match(&out) {
            continue;
        }
        let replaced = rule.re.replace_all(&out, |c: &Captures<'_>| {
            let whole = c.get(0).expect("match");
            let Some(value) = c.get(rule.group) else {
                return whole.as_str().to_owned();
            };
            if rule.group > 0 && is_benign_bare(value.as_str()) {
                return whole.as_str().to_owned();
            }
            let s = whole.as_str();
            let (a, b) = (value.start() - whole.start(), value.end() - whole.start());
            format!("{}[REDACTED:{}]{}", &s[..a], rule.kind, &s[b..])
        });
        if let Cow::Owned(s) = replaced {
            out = Cow::Owned(s);
        }
    }
    out
}

fn is_base64_line(line: &str) -> bool {
    line.len() >= 16
        && line
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
}

/// Which lines are private key material. A chunk or passage can start or
/// end inside a key block, or lie entirely within a long one: besides the
/// lines between `-----BEGIN ... PRIVATE KEY-----` and `-----END`, the
/// base64 lines right before an `END` with no `BEGIN` in view, and any run
/// of three or more 64-column base64 lines (the PEM body width) count.
fn key_lines(lines: &[&str]) -> Vec<bool> {
    let mut key = vec![false; lines.len()];
    // Certificates and public keys are not secrets.
    let mut public = vec![false; lines.len()];
    let mut in_key = false;
    let mut in_public = false;
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        let marker = t.starts_with("-----") && t.contains("PRIVATE KEY");
        if t.starts_with("-----BEGIN") && !marker {
            in_public = true;
        } else if t.starts_with("-----END") && !marker {
            in_public = false;
        } else if in_public {
            public[i] = true;
        }
        if marker && t.starts_with("-----BEGIN") {
            in_key = !t.contains("-----END");
        } else if marker && t.starts_with("-----END") {
            if !in_key {
                let mut j = i;
                while j > 0 && is_base64_line(lines[j - 1].trim()) {
                    j -= 1;
                    key[j] = true;
                }
            }
            in_key = false;
        } else if in_key {
            // Blank lines and PEM headers (Proc-Type: ...) carry no key.
            key[i] = !t.is_empty() && !t.contains(':');
        }
    }
    let mut i = 0;
    while i < lines.len() {
        let pem_width = |l: &&str| l.trim().len() == 64 && is_base64_line(l.trim());
        let run = lines[i..]
            .iter()
            .zip(&public[i..])
            .take_while(|(l, p)| !**p && pem_width(l))
            .count();
        if run >= 3 {
            key[i..i + run].iter_mut().for_each(|k| *k = true);
        }
        i += run.max(1);
    }
    key
}

/// Replaces every recognised secret value in `text` with
/// `[REDACTED:<kind>]`, line by line. The line count never changes, and a
/// text without secrets is returned borrowed.
pub fn redact(text: &str) -> Cow<'_, str> {
    if text.is_empty() {
        return Cow::Borrowed(text);
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let key = key_lines(&lines);
    let mut changed = false;
    let mut out = String::with_capacity(text.len());
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if key[i] {
            let indent = &line[..line.len() - line.trim_start().len()];
            out.push_str(indent);
            out.push_str("[REDACTED:");
            out.push_str(PRIVATE_KEY);
            out.push(']');
            changed = true;
            continue;
        }
        let r = redact_line(line);
        if let Cow::Owned(_) = r {
            changed = true;
        }
        out.push_str(&r);
    }
    if changed {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(text)
    }
}

/// [`redact`] in place.
pub fn redact_in_place(text: &mut String) {
    if let Cow::Owned(s) = redact(text) {
        *text = s;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: &str) -> String {
        redact(s).into_owned()
    }

    #[test]
    fn token_formats() {
        let aws = format!("key = {}", ["AKIA", "IOSFODNN7EXAMPLE"].concat());
        assert_eq!(r(&aws), "key = [REDACTED:aws_access_key]");
        let gh = format!("GITHUB_TOKEN {}", ["ghp_", &"a".repeat(36)].concat());
        assert_eq!(r(&gh), "GITHUB_TOKEN [REDACTED:github_token]");
        let gl = format!("use {} here", ["glpat-", "abcdefghij0123456789"].concat());
        assert_eq!(r(&gl), "use [REDACTED:gitlab_token] here");
        let slack = ["xoxb-", "1234567890-abcdefghij"].concat();
        assert_eq!(r(&slack), "[REDACTED:slack_token]");
        let jwt =
            "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NSJ9.abcdefghijklmnop";
        assert!(r(jwt).contains("[REDACTED:jwt]"), "{}", r(jwt));
    }

    #[test]
    fn assignments_redact_only_literals() {
        assert_eq!(
            r(r#"db_password = "hunter2hunter""#),
            r#"db_password = "[REDACTED:assignment]""#
        );
        assert_eq!(
            r(r#"  "apiKey": "abcd1234efgh","#),
            r#"  "apiKey": "[REDACTED:assignment]","#
        );
        assert_eq!(
            r("DB_PASSWORD=s3cr3t!"),
            "DB_PASSWORD=[REDACTED:assignment]"
        );
        assert_eq!(
            r("run with GITHUB_TOKEN=abc123def456 set"),
            "run with GITHUB_TOKEN=[REDACTED:assignment] set"
        );
        assert_eq!(
            r("export API_KEY=abcdef123"),
            "export API_KEY=[REDACTED:assignment]"
        );
        assert_eq!(
            r("  client_secret: verysecret"),
            "  client_secret: [REDACTED:assignment]"
        );
        // Expressions, references and placeholders stay.
        for code in [
            "let password = read_password();",
            "password = request.form['password']",
            "token = self.token",
            "API_KEY=${API_KEY}",
            "env DB_PASSWORD=$PW ./run",
            "secret: true",
            "def check_password(password):",
            "fn token(&self) -> &str {",
        ] {
            assert_eq!(r(code), code, "{code}");
        }
    }

    #[test]
    fn connection_strings_and_urls() {
        assert_eq!(
            r("Server=db;Database=app;User Id=sa;Password=P@ssw0rd!;"),
            "Server=db;Database=app;User Id=sa;Password=[REDACTED:connection_string];"
        );
        assert_eq!(
            r(r#""Default": "Server=x;Pwd=abc123;""#),
            r#""Default": "Server=x;Pwd=[REDACTED:connection_string];""#
        );
        assert_eq!(
            r("postgres://app:topsecret@db:5432/app"),
            "postgres://app:[REDACTED:url_credentials]@db:5432/app"
        );
        assert_eq!(r("https://example.com/a:b"), "https://example.com/a:b");
    }

    #[test]
    fn private_key_blocks_keep_their_lines() {
        let pem =
            "a\n-----BEGIN RSA PRIVATE KEY-----\nMIIEow\n  QUJD\n-----END RSA PRIVATE KEY-----\nb";
        let out = r(pem);
        assert_eq!(
            out,
            "a\n-----BEGIN RSA PRIVATE KEY-----\n[REDACTED:private_key]\n  [REDACTED:private_key]\n-----END RSA PRIVATE KEY-----\nb"
        );
        assert_eq!(out.lines().count(), pem.lines().count());
        // A chunk that starts or lies inside a key block.
        let tail = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVo=\n-----END PRIVATE KEY-----";
        assert_eq!(r(tail), "[REDACTED:private_key]\n-----END PRIVATE KEY-----");
        let body = ["A".repeat(64), "B".repeat(64), "C".repeat(64)].join("\n");
        assert_eq!(r(&body), ["[REDACTED:private_key]"; 3].join("\n"));
        // Public keys and certificates are not secrets.
        let cert = format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----",
            ["A".repeat(64), "B".repeat(64), "C".repeat(64)].join("\n")
        );
        let cert = cert.as_str();
        assert_eq!(r(cert), cert);
    }

    #[test]
    fn plain_text_is_borrowed_and_idempotent() {
        let text = "fn main() {\n    println!(\"hello\");\n}\n";
        assert!(matches!(redact(text), Cow::Borrowed(_)));
        let once = r("DB_PASSWORD=s3cr3t!\npassword = \"abcdefgh\"");
        assert_eq!(r(&once), once);
    }

    #[test]
    fn references_to_other_settings_are_not_secrets() {
        for line in [
            "secret: this.secret",
            "password = process.env.DB_PASSWORD",
            "db_password: os.environment",
            "api_key: env.API_KEY",
            "token: required",
        ] {
            assert_eq!(r(line), line, "{line}");
        }
    }

    #[test]
    fn key_blocks_keep_blank_and_header_lines() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\n\nMIIEow\n-----END RSA PRIVATE KEY-----";
        assert_eq!(
            r(pem),
            "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\n\n[REDACTED:private_key]\n-----END RSA PRIVATE KEY-----"
        );
        // Before an END whose BEGIN is out of view, only the base64 lines
        // right above it are key material: prose and short tokens stay.
        let tail = "Rotate the deploy key monthly.\nabc\nQUJDREVGR0hJSktMTU5PUFFS\n-----END PRIVATE KEY-----";
        assert_eq!(
            r(tail),
            "Rotate the deploy key monthly.\nabc\n[REDACTED:private_key]\n-----END PRIVATE KEY-----"
        );
    }

    #[test]
    fn only_runs_of_64_column_base64_lines_count_as_key_bodies() {
        let short = ["QUJDREVGR0hJSktMTU5P"; 3].join("\n");
        assert_eq!(r(&short), short);
        let prose = ["The quick brown fox jumps over the lazy dog, again and again!!"; 3]
            .map(|l| format!("{l:<64}"))
            .join("\n");
        assert_eq!(r(&prose), prose);
    }

    #[test]
    fn redact_in_place_rewrites_only_when_needed() {
        let mut text = String::from("DB_PASSWORD=s3cr3t!");
        redact_in_place(&mut text);
        assert_eq!(text, "DB_PASSWORD=[REDACTED:assignment]");
        let mut plain = String::from("nothing to see");
        redact_in_place(&mut plain);
        assert_eq!(plain, "nothing to see");
    }
}
