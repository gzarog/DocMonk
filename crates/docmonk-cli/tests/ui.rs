//! `ragmonk ui` end to end.
//!
//! Replays `fixtures/expected/ui.json` against the server on the fixture
//! corpus: status codes, content types, `HX-Redirect` targets and bodies
//! with paths, ids, timestamps and sizes normalized.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::Value;

const FIXTURES: &[(&str, &str)] = &[
    ("fixtures/corpus/languages", "code"),
    ("fixtures/corpus/documents/simple.md", "docs/simple.md"),
    ("fixtures/corpus/documents/simple.txt", "docs/simple.txt"),
    ("fixtures/corpus/documents/simple.csv", "docs/simple.csv"),
    ("fixtures/corpus/documents/simple.html", "docs/simple.html"),
    (
        "fixtures/corpus/documents/email_with_attachments.eml",
        "docs/email_with_attachments.eml",
    ),
];

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn copy(from: &Path, to: &Path) {
    if from.is_dir() {
        std::fs::create_dir_all(to).unwrap();
        for e in std::fs::read_dir(from).unwrap() {
            let e = e.unwrap();
            copy(&e.path(), &to.join(e.file_name()));
        }
    } else {
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(from, to).unwrap();
    }
}

fn ragmonk(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_ragmonk"));
    for (k, _) in std::env::vars() {
        if k.starts_with("RAGMONK_") || k == "OPENAI_API_KEY" || k == "ANTHROPIC_API_KEY" {
            c.env_remove(k);
        }
    }
    c.env("RAGMONK_HOME", home)
        .env("RAGMONK_SEARCH__SEMANTIC", "false")
        .env("RAGMONK_UPDATES__ENABLED", "false");
    c
}

/// Every corpus file gets a fixed mtime, so search ties break the same
/// way on every run.
fn pin_mtimes(dir: &Path) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            pin_mtimes(&p);
        } else {
            let t = std::time::UNIX_EPOCH + Duration::from_secs(1_767_225_600);
            std::fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(t)
                .unwrap();
        }
    }
}

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

fn dechunk(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = raw;
    loop {
        let Some(pos) = rest.windows(2).position(|w| w == b"\r\n") else {
            break;
        };
        let size = usize::from_str_radix(std::str::from_utf8(&rest[..pos]).unwrap().trim(), 16)
            .unwrap_or(0);
        if size == 0 {
            break;
        }
        let start = pos + 2;
        out.extend_from_slice(&rest[start..start + size]);
        rest = &rest[start + size + 2..];
    }
    out
}

fn request(port: u16, method: &str, path: &str, headers: &[(String, String)], body: &str) -> Resp {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nconnection: close\r\n");
    if !headers.iter().any(|(k, _)| k == "host") {
        req.push_str(&format!("host: 127.0.0.1:{port}\r\n"));
    }
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if method == "POST" {
        req.push_str("content-type: application/x-www-form-urlencoded\r\n");
        req.push_str(&format!("content-length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.lines();
    let status: u16 = lines
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_owned()))
        .collect();
    let mut body = raw[split + 4..].to_vec();
    if headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.contains("chunked"))
    {
        body = dechunk(&body);
    }
    Resp {
        status,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b' ' => "+".into(),
            b => format!("%{b:02X}"),
        })
        .collect()
}

#[test]
fn admin_ui_is_as_expected() {
    let golden_path = repo().join("fixtures/expected/ui.json");
    let mut golden: Vec<Value> =
        serde_json::from_str(&std::fs::read_to_string(&golden_path).unwrap()).unwrap();
    // `RAGMONK_BLESS=1` rewrites the expected pages from this build's output.
    let bless = std::env::var_os("RAGMONK_BLESS").is_some();
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let corpus = dir.path().join("source");
    for (from, to) in FIXTURES {
        copy(&repo().join(from), &corpus.join(to));
    }
    pin_mtimes(&corpus);
    let run = |args: &[&str]| {
        let o = ragmonk(&home)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(o.status.success(), "{args:?}: {o:?}");
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    run(&["init"]);
    let added = run(&["source", "add", corpus.to_str().unwrap()]);
    let at = added.find("src_").unwrap();
    let source_id: String = added[at..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    run(&["index"]);

    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let _server = Server(
        ragmonk(&home)
            .args(["ui", "--no-browser", "--port", &port.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "server did not start");
        std::thread::sleep(Duration::from_millis(100));
    }

    // The CSRF cookie minted on a first visit, then sent on every request.
    let first = request(port, "GET", "/sources", &[], "");
    let token = first
        .header("set-cookie")
        .and_then(|c| c.split(';').next())
        .and_then(|kv| kv.strip_prefix("ragmonk_csrf="))
        .expect("csrf cookie")
        .to_owned();
    let docs = request(
        port,
        "GET",
        &format!("/documents?source_id={source_id}&q=simple.md"),
        &[(String::from("cookie"), format!("ragmonk_csrf={token}"))],
        "",
    )
    .body;
    let doc_re = Regex::new(&format!("/documents/{source_id}/([0-9a-f]+)")).unwrap();
    let doc_id = doc_re.captures(&docs).expect("document link")[1].to_owned();

    let version = env!("CARGO_PKG_VERSION");
    let build_version = option_env!("RAGMONK_BUILD_VERSION")
        .filter(|v| !v.is_empty())
        .unwrap_or(version);
    let mut subs: Vec<(String, String)> = vec![
        (source_id.clone(), "<SOURCE>".into()),
        (token.clone(), "<CSRF>".into()),
        (format!("v{build_version}"), "v<VERSION>".into()),
        (build_version.to_owned(), "<VERSION>".into()),
    ];
    for (p, label) in [(&corpus, "<CORPUS>"), (&home, "<HOME>")] {
        subs.push((p.to_string_lossy().into_owned(), label.into()));
        if let Ok(real) = std::fs::canonicalize(p) {
            let real = real.to_string_lossy().into_owned();
            subs.push((real.trim_start_matches(r"\\?\").to_owned(), label.into()));
            subs.push((real, label.into()));
        }
    }
    subs.sort_by_key(|(n, _)| std::cmp::Reverse(n.len()));
    let ts = Regex::new(r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?")
        .unwrap();
    let hex = Regex::new(r"\b[0-9a-f]{32}\b").unwrap();
    let size = Regex::new(r"\d+(?:\.\d+)? (?:[KMGT]?B)\b").unwrap();
    let ws = Regex::new(r"\s+").unwrap();
    let between = Regex::new(r">\s+<").unwrap();
    let normalize = |text: &str| -> String {
        let mut t = text.to_owned();
        for (needle, repl) in &subs {
            t = t.replace(needle.as_str(), repl);
        }
        // Windows paths are compared in their slash form.
        let t = t.replace('\\', "/");
        let t = ts.replace_all(&t, "<TS>");
        let t = hex.replace_all(&t, "<ID>");
        let t = size.replace_all(&t, "<N> B");
        let t = ws.replace_all(&t, " ");
        between.replace_all(&t, "><").trim().to_owned()
    };
    let golden_size = Regex::new(r"(?:<N> KB|\d+(?:\.\d+)? (?:[KMGT]?B)\b)").unwrap();
    // The schema fingerprint changes with every schema edit.
    let schema = Regex::new(r"schema [0-9a-f]{16}").unwrap();
    // Windows checkouts convert the fixtures to CRLF, so file sizes in the
    // Documents table differ from the expected (POSIX) ones there.
    let doc_size = Regex::new(r"</td><td>\d+</td><td>(indexed|failed|queued|—)").unwrap();
    let doc_size = |t: &str| -> String {
        if cfg!(windows) {
            doc_size
                .replace_all(t, "</td><td><SIZE></td><td>$1")
                .into_owned()
        } else {
            t.to_owned()
        }
    };

    let mut failures = Vec::new();
    for rec in golden.iter_mut() {
        let method = rec["method"].as_str().unwrap();
        if method == "WRITE_LOG" {
            let log = home.join("logs").join("ragmonk.log");
            std::fs::create_dir_all(log.parent().unwrap()).unwrap();
            let lines: Vec<&str> = rec["log_lines"]
                .as_array()
                .unwrap()
                .iter()
                .map(|l| l.as_str().unwrap())
                .collect();
            std::fs::write(&log, format!("{}\n", lines.join("\n"))).unwrap();
            continue;
        }
        let path = rec["path"]
            .as_str()
            .unwrap()
            .replace("{source}", &source_id)
            .replace("{doc}", &doc_id);
        let mut headers: Vec<(String, String)> =
            vec![("cookie".into(), format!("ragmonk_csrf={token}"))];
        for (k, v) in rec["headers"].as_object().unwrap() {
            headers.push((k.clone(), v.as_str().unwrap().to_owned()));
        }
        if rec["csrf"] == true {
            headers.push(("x-csrf-token".into(), token.clone()));
        }
        let body = rec["form"]
            .as_object()
            .map(|f| {
                f.iter()
                    .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v.as_str().unwrap())))
                    .collect::<Vec<_>>()
                    .join("&")
            })
            .unwrap_or_default();
        let url = path.replace(' ', "+");
        let r = request(port, method, &url, &headers, &body);
        let label = format!("{method} {}", rec["path"].as_str().unwrap());
        let ctype = r
            .header("content-type")
            .unwrap_or_default()
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned();
        if !bless && u64::from(r.status) != rec["status"].as_u64().unwrap() {
            failures.push(format!(
                "{label}: status {} != {}\n{}",
                r.status, rec["status"], r.body
            ));
            continue;
        }
        if ctype != rec["content_type"].as_str().unwrap() {
            failures.push(format!(
                "{label}: content-type {ctype} != {}",
                rec["content_type"]
            ));
        }
        let hx = r.header("hx-redirect").map(&normalize);
        if hx.as_deref() != rec["hx_redirect"].as_str() {
            failures.push(format!(
                "{label}: hx-redirect {hx:?} != {}",
                rec["hx_redirect"]
            ));
        }
        if bless {
            rec["status"] = r.status.into();
            rec["content_type"] = ctype.clone().into();
            rec["hx_redirect"] = hx.clone().map_or(Value::Null, Value::from);
            rec["body"] = if rec["body"].is_string() {
                schema
                    .replace_all(&normalize(&r.body), "schema <FP>")
                    .into_owned()
                    .into()
            } else {
                serde_json::from_str(&normalize(&r.body)).unwrap_or(Value::Null)
            };
            continue;
        }
        let ok = match &rec["body"] {
            Value::String(expected) => {
                let expected = golden_size.replace_all(expected, "<N> B");
                let expected = schema.replace_all(&expected, "schema <FP>");
                let got = normalize(&r.body);
                let got = doc_size(&schema.replace_all(&got, "schema <FP>"));
                let expected = doc_size(&expected);
                if got != expected {
                    let at = got
                        .chars()
                        .zip(expected.chars())
                        .take_while(|(a, b)| a == b)
                        .count();
                    let from = at.saturating_sub(80);
                    let got_tail: String = got.chars().skip(from).take(300).collect();
                    let exp_tail: String = expected.chars().skip(from).take(300).collect();
                    failures.push(format!(
                        "{label}: body differs at char {at}\n  got      …{got_tail}\n  expected …{exp_tail}"
                    ));
                }
                true
            }
            expected => {
                let got: Value = serde_json::from_str(&normalize(&r.body)).unwrap_or(Value::Null);
                got == *expected
            }
        };
        if !ok {
            failures.push(format!("{label}: json body {} != {}", r.body, rec["body"]));
        }
    }
    if bless {
        let mut out = Vec::new();
        let fmt = serde_json::ser::PrettyFormatter::with_indent(b" ");
        serde::Serialize::serialize(
            &golden,
            &mut serde_json::Serializer::with_formatter(&mut out, fmt),
        )
        .unwrap();
        out.push(b'\n');
        std::fs::write(&golden_path, out).unwrap();
        return;
    }
    assert!(
        failures.is_empty(),
        "{} failure(s):\n{}",
        failures.len(),
        failures.join("\n\n")
    );

    // A saved configuration shows at once, without a restart.
    let cfg = request(
        port,
        "GET",
        "/config",
        &[("cookie".into(), format!("ragmonk_csrf={token}"))],
        "",
    );
    assert!(
        cfg.body.contains(r#"name="context__max_files" value="7""#),
        "{}",
        cfg.body
    );
}

/// Reads an SSE stream until `needle` shows up (or the deadline).
fn read_stream_until(port: u16, path: &str, needle: &str, timeout: Duration) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    s.write_all(
        format!("GET {path} HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\nconnection: close\r\n\r\n")
            .as_bytes(),
    )
    .unwrap();
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => seen.extend_from_slice(&buf[..n]),
            Err(_) => {}
        }
        if String::from_utf8_lossy(&seen).contains(needle) {
            break;
        }
    }
    String::from_utf8_lossy(&seen).into_owned()
}

#[test]
fn admin_ui_actions() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.py"), "def alpha():\n    return 1\n").unwrap();
    let run = |args: &[&str]| {
        let o = ragmonk(&home)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(o.status.success(), "{args:?}: {o:?}");
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    run(&["init"]);
    let added = run(&["source", "add", src.to_str().unwrap()]);
    let at = added.find("src_").unwrap();
    let source_id: String = added[at..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let _server = Server(
        ragmonk(&home)
            .args(["ui", "--no-browser", "--port", &port.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "server did not start");
        std::thread::sleep(Duration::from_millis(100));
    }
    let token = request(port, "GET", "/", &[], "")
        .header("set-cookie")
        .and_then(|c| c.split(';').next())
        .and_then(|kv| kv.strip_prefix("ragmonk_csrf="))
        .unwrap()
        .to_owned();
    let auth = vec![
        ("cookie".to_owned(), format!("ragmonk_csrf={token}")),
        ("x-csrf-token".to_owned(), token.clone()),
    ];

    // Background indexing, watched over the event stream.
    let r = request(port, "POST", "/indexing/start", &auth, "");
    assert_eq!(
        (r.status, r.header("hx-redirect")),
        (204, Some("/indexing"))
    );
    let events = read_stream_until(
        port,
        "/events/indexing",
        "run_completed",
        Duration::from_secs(120),
    );
    assert!(events.contains("event: run_started"), "{events}");
    assert!(events.contains("event: scan_completed"), "{events}");
    assert!(events.contains("event: run_completed"), "{events}");
    assert!(events.contains("text/event-stream"), "{events}");
    let page = request(port, "GET", "/indexing", &auth[..1], "").body;
    assert!(
        page.contains("<code>index</code>"),
        "last run summary: {page}"
    );
    let symbols = request(port, "GET", "/knowledge?q=alpha", &auth[..1], "").body;
    assert!(symbols.contains("a.alpha"), "{symbols}");

    // A refused second start while one runs is covered by the runner's
    // guard; a CSRF-less POST is refused outright.
    let r = request(port, "POST", "/indexing/rebuild", &auth[..1], "fresh=1");
    assert_eq!(r.status, 403);

    // Backups: create, list, download, restore.
    let r = request(port, "POST", "/backups", &auth, "");
    assert_eq!(
        r.header("hx-redirect"),
        Some("/backups?saved=1"),
        "{}",
        r.body
    );
    let page = request(port, "GET", "/backups", &auth[..1], "").body;
    let name = Regex::new(r#"ragmonk-backup-[^\s<"]+\.tar\.gz"#)
        .unwrap()
        .find(&page)
        .expect("listed backup")
        .as_str()
        .to_owned();
    let dl = request(
        port,
        "GET",
        &format!("/backups/download/{name}"),
        &auth[..1],
        "",
    );
    assert_eq!(dl.status, 200);
    assert_eq!(dl.header("content-type"), Some("application/gzip"));
    let r = request(
        port,
        "POST",
        "/backups/restore",
        &auth,
        &format!("name={}", urlencode(&name)),
    );
    assert_eq!(
        r.header("hx-redirect"),
        Some("/backups?saved=restored"),
        "{:?}",
        r.headers
    );

    // Removing a source deletes its indexed data, not its files.
    let r = request(
        port,
        "POST",
        &format!("/sources/{source_id}/remove"),
        &auth,
        "",
    );
    assert_eq!(r.header("hx-redirect"), Some("/sources"));
    let page = request(port, "GET", "/sources", &auth[..1], "").body;
    assert!(page.contains("No sources registered yet."), "{page}");
    assert!(src.join("a.py").is_file());

    // Static assets are embedded.
    let css = request(port, "GET", "/static/css/app.css", &[], "");
    assert_eq!(
        (css.status, css.header("content-type")),
        (200, Some("text/css; charset=utf-8"))
    );
    assert_eq!(request(port, "GET", "/static/nope.js", &[], "").status, 404);
}

fn start_ui(home: &Path, env: &[(&str, &str)]) -> (Server, u16) {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut c = ragmonk(home);
    for (k, v) in env {
        c.env(k, v);
    }
    let server = Server(
        c.args(["ui", "--no-browser", "--port", &port.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "server did not start");
        std::thread::sleep(Duration::from_millis(100));
    }
    (server, port)
}

/// `/api/status` is the CLI's canonical report; `/ready` reflects the
/// backend while `/health` is only process liveness.
#[test]
fn status_api_matches_cli_and_readiness_tracks_the_backend() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let corpus = dir.path().join("source");
    std::fs::create_dir_all(&corpus).unwrap();
    std::fs::write(corpus.join("app.py"), "def main():\n    return 1\n").unwrap();
    for args in [
        &["init"][..],
        &["source", "add", corpus.to_str().unwrap()],
        &["index"],
    ] {
        assert!(ragmonk(&home).args(args).output().unwrap().status.success());
    }
    let cli: Value = serde_json::from_slice(
        &ragmonk(&home)
            .args(["status", "--json"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let cli = &cli["data"];
    {
        let (_server, port) = start_ui(&home, &[]);
        let api: Value =
            serde_json::from_str(&request(port, "GET", "/api/status", &[], "").body).unwrap();
        for k in ["mode", "health", "summary", "problems", "recent_errors"] {
            assert_eq!(api[k], cli[k], "{k}");
        }
        let states = |v: &Value| -> Vec<(Value, Value)> {
            v["sources"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| (s["source_id"].clone(), s["index_state"].clone()))
                .collect()
        };
        assert_eq!(states(&api), states(cli));
        let ready = request(port, "GET", "/ready", &[], "");
        assert_eq!(ready.status, 200, "{}", ready.body);
        let ready: Value = serde_json::from_str(&ready.body).unwrap();
        assert_eq!(ready["ready"], true);
        assert_eq!(ready["health"], "healthy");
    }
    // An unreachable server: alive, but not ready, and no report.
    let (_server, port) = start_ui(
        &home,
        &[
            ("RAGMONK_STORAGE__MODE", "server"),
            ("RAGMONK_STORAGE__SERVER__URL", "http://127.0.0.1:9"),
            ("RAGMONK_STORAGE__SERVER__REQUEST_TIMEOUT_SECONDS", "2"),
        ],
    );
    assert_eq!(request(port, "GET", "/health", &[], "").status, 200);
    let ready = request(port, "GET", "/ready", &[], "");
    assert_eq!(ready.status, 503, "{}", ready.body);
    let ready: Value = serde_json::from_str(&ready.body).unwrap();
    assert_eq!(ready["ready"], false);
    assert_eq!(ready["error"]["type"], "BackendUnavailableError");
    assert_eq!(request(port, "GET", "/api/status", &[], "").status, 503);
}
