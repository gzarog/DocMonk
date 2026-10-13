//! `ragmonk serve --mcp` end to end.
//!
//! Replays the stdio session in `fixtures/expected/mcp.json` against the
//! fixture corpus, normalizing paths, ids and timestamps in responses.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Map, Value};

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
const DROP: &[&str] = &[
    "entity_id",
    "source_entity_id",
    "target_entity_id",
    "file_id",
    "id",
    "parent_document_id",
];
const VOLATILE: &[&str] = &[
    "pid",
    "hostname",
    "host",
    "snapshot_id",
    "run_id",
    "active_id",
    "base_build_id",
    "request_count",
    "resources",
];
const VOLATILE_SUFFIXES: &[&str] = &[
    "_at",
    "_age_seconds",
    "_ms",
    "duration_seconds",
    "elapsed_seconds",
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
    c.env("RAGMONK_HOME", home)
        .env("RAGMONK_SEARCH__SEMANTIC", "false")
        .env("RAGMONK_UPDATES__ENABLED", "false");
    c
}

fn run(home: &Path, args: &[&str]) -> String {
    let o = ragmonk(home)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(o.status.success(), "{args:?}: {o:?}");
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn timestamp(s: &str) -> String {
    // \d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:?\d{2})?
    let b = s.as_bytes();
    let digits =
        |i: usize, n: usize| i + n <= b.len() && b[i..i + n].iter().all(u8::is_ascii_digit);
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        let head = digits(i, 4)
            && b.get(i + 4) == Some(&b'-')
            && digits(i + 5, 2)
            && b.get(i + 7) == Some(&b'-')
            && digits(i + 8, 2)
            && matches!(b.get(i + 10), Some(b'T' | b' '))
            && digits(i + 11, 2)
            && b.get(i + 13) == Some(&b':')
            && digits(i + 14, 2)
            && b.get(i + 16) == Some(&b':')
            && digits(i + 17, 2);
        if !head {
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let mut j = i + 19;
        if b.get(j) == Some(&b'.') && digits(j + 1, 1) {
            j += 1;
            while digits(j, 1) {
                j += 1;
            }
        }
        if b.get(j) == Some(&b'Z') {
            j += 1;
        } else if matches!(b.get(j), Some(b'+' | b'-')) && digits(j + 1, 2) {
            let k = if b.get(j + 3) == Some(&b':') {
                j + 4
            } else {
                j + 3
            };
            if digits(k, 2) {
                j = k + 2;
            }
        }
        out.push_str("<TS>");
        i = j;
    }
    out
}

/// Compact key-sorted JSON text (a canonical form for unordered lists).
fn sorted_text(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .iter()
                .map(|k| format!("{}:{}", Value::String((*k).clone()), sorted_text(&m[*k])))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(sorted_text).collect::<Vec<_>>().join(",")
        ),
        other => other.to_string(),
    }
}

fn sorted(mut v: Vec<Value>) -> Vec<Value> {
    v.sort_by_key(sorted_text);
    v
}

/// Replaces volatile values (paths, ids, timestamps) with placeholders and
/// sorts the lists named in `unordered`.
fn normalize(v: &Value, subs: &[(String, &str)], unordered: &[String]) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, inner) in map {
                if DROP.contains(&k.as_str()) {
                    continue;
                }
                if !inner.is_null()
                    && (VOLATILE.contains(&k.as_str())
                        || VOLATILE_SUFFIXES.iter().any(|s| k.ends_with(s)))
                {
                    out.insert(k.clone(), Value::String("<VOLATILE>".into()));
                    continue;
                }
                let mut n = normalize(inner, subs, unordered);
                if unordered.contains(k) {
                    if let Value::Array(a) = n {
                        n = Value::Array(sorted(a));
                    }
                }
                out.insert(k.clone(), n);
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| normalize(x, subs, unordered)).collect()),
        Value::String(s) => {
            // Slashes first, so Windows paths match the slash-normalized
            // needles.
            let mut s = s.replace('\\', "/");
            for (needle, repl) in subs {
                s = s.replace(needle.as_str(), repl);
            }
            Value::String(timestamp(&s))
        }
        other => other.clone(),
    }
}

/// JSON equality with numbers compared as floats (120 == 120.0).
fn same(a: &Value, b: &Value, path: &str) -> Result<(), String> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            if (x.as_f64().unwrap() - y.as_f64().unwrap()).abs() < 1e-9 {
                Ok(())
            } else {
                Err(format!("{path}: {x} != {y}"))
            }
        }
        (Value::Object(x), Value::Object(y)) => {
            let kx: Vec<_> = x.keys().collect();
            let ky: Vec<_> = y.keys().collect();
            if kx != ky {
                return Err(format!("{path}: keys {kx:?} != {ky:?}"));
            }
            for (k, v) in x {
                same(v, &y[k], &format!("{path}.{k}"))?;
            }
            Ok(())
        }
        (Value::Array(x), Value::Array(y)) => {
            if x.len() != y.len() {
                return Err(format!("{path}: length {} != {}", x.len(), y.len()));
            }
            for (i, (p, q)) in x.iter().zip(y).enumerate() {
                same(p, q, &format!("{path}[{i}]"))?;
            }
            Ok(())
        }
        _ if a == b => Ok(()),
        _ => Err(format!("{path}: {a} != {b}")),
    }
}

#[test]
fn mcp_session_is_as_expected() {
    let golden_path = repo().join("fixtures/expected/mcp.json");
    let mut golden: Vec<Value> =
        serde_json::from_str(&std::fs::read_to_string(&golden_path).unwrap()).unwrap();
    // `RAGMONK_BLESS=1` rewrites the expected responses from this build.
    let bless = std::env::var_os("RAGMONK_BLESS").is_some();
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let corpus = dir.path().join("source");
    for (from, to) in FIXTURES {
        copy(&repo().join(from), &corpus.join(to));
    }
    run(&home, &["init"]);
    let added = run(&home, &["source", "add", corpus.to_str().unwrap()]);
    let at = added.find("src_").expect("source id");
    let source_id: String = added[at..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    run(&home, &["index"]);

    let mut subs: Vec<(String, &str)> = vec![
        (corpus.to_string_lossy().into_owned(), "<CORPUS>"),
        (home.to_string_lossy().into_owned(), "<HOME>"),
        (source_id, "<SOURCE>"),
    ];
    for (p, label) in [(&corpus, "<CORPUS>"), (&home, "<HOME>")] {
        if let Ok(real) = std::fs::canonicalize(p) {
            subs.push((real.to_string_lossy().into_owned(), label));
        }
    }
    // Needles in slash-normalized form, with and without the Windows
    // verbatim prefix.
    let mut subs: Vec<(String, &str)> = subs
        .into_iter()
        .flat_map(|(n, l)| {
            let n = n.replace('\\', "/");
            let short = n.strip_prefix("//?/").map(str::to_owned);
            std::iter::once((n, l)).chain(short.map(|s| (s, l)))
        })
        .collect();
    subs.sort_by_key(|(n, _)| std::cmp::Reverse(n.len()));

    let mut child = ragmonk(&home)
        .args(["serve", "--mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut failures = Vec::new();
    for record in golden.iter_mut() {
        let request = record["request"].clone();
        writeln!(stdin, "{}", serde_json::to_string(&request).unwrap()).unwrap();
        stdin.flush().unwrap();
        if record["response"].is_null() {
            continue;
        }
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let mut response: Value = serde_json::from_str(&line).expect(&line);
        let method = request["method"].as_str().unwrap();
        let unordered: Vec<String> = record["unordered"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|u| u.as_str().map(str::to_owned))
            .collect();
        let actual = if method == "tools/list" {
            response
        } else {
            if let Some(result) = response.get_mut("result") {
                if method == "initialize" {
                    assert!(result["serverInfo"]["name"]
                        .as_str()
                        .unwrap()
                        .starts_with("ragmonk v"));
                    result["serverInfo"]["name"] = "ragmonk v<VERSION>".into();
                    result["serverInfo"]["version"] = "<VERSION>".into();
                }
                if let Some(structured) = result.get("structuredContent") {
                    let text = result["content"][0]["text"].as_str().unwrap();
                    assert_eq!(&serde_json::from_str::<Value>(text).unwrap(), structured);
                    result["content"][0]["text"] = "<structuredContent>".into();
                }
            }
            normalize(&response, &subs, &unordered)
        };
        let label = format!(
            "{method} {}",
            serde_json::to_string(&request["params"]).unwrap()
        );
        if bless {
            record["response"] = actual;
            continue;
        }
        if let Err(e) = same(&actual, &record["response"], "") {
            failures.push(format!(
                "{label}\n  {e}\n  actual:   {}\n  expected: {}",
                serde_json::to_string(&actual).unwrap(),
                serde_json::to_string(&record["response"]).unwrap()
            ));
        }
    }
    drop(stdin);
    let status = child.wait().unwrap();
    assert!(status.success());
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
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn serve_requires_mcp_and_respects_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let o = ragmonk(&home)
        .arg("serve")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("requires --mcp"));
    let o = ragmonk(&home)
        .args(["serve", "--mcp"])
        .env("RAGMONK_MCP__ENABLED", "false")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("mcp.enabled=false"));
    // EOF on stdin ends the server cleanly; nothing is written to stdout.
    let o = ragmonk(&home)
        .args(["serve", "--mcp"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(o.status.success(), "{o:?}");
    assert!(o.stdout.is_empty());
    assert!(String::from_utf8_lossy(&o.stderr).contains("RagMonk MCP server starting on stdio..."));
}
