//! `ask` and `ai …` end to end.
//!
//! Replays `fixtures/expected/ai_cli.json`: the fixture corpus indexed by
//! the binary, `ai.provider=ollama` pointed at a local mock, and no `codex`
//! on `PATH`. Exit codes, stdout and the provider request must match;
//! stderr matches up to whitespace.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

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

type Shared = Arc<Mutex<(u16, String, Vec<Value>)>>;

/// An HTTP mock answering every POST with the current response and
/// recording `{path, body}`.
fn mock() -> (u16, Shared) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let state: Shared = Arc::new(Mutex::new((200, String::new(), Vec::new())));
    let shared = state.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let path = line
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_owned();
            let mut length = 0usize;
            loop {
                let mut h = String::new();
                reader.read_line(&mut h).unwrap();
                let h = h.trim_end().to_lowercase();
                if h.is_empty() {
                    break;
                }
                if let Some(v) = h.strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).unwrap();
            let (status, text) = {
                let mut s = shared.lock().unwrap();
                s.2.push(serde_json::json!({
                    "path": path,
                    "body": serde_json::from_slice::<Value>(&body).unwrap(),
                }));
                (s.0, s.1.clone())
            };
            let reply = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                text.len()
            );
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    (port, state)
}

struct Norm {
    subs: Vec<(String, String)>,
}

impl Norm {
    fn text(&self, s: &str) -> String {
        let mut s = s.replace('\\', "/");
        for (needle, repl) in &self.subs {
            s = s.replace(needle.as_str(), repl);
        }
        s
    }

    fn value(&self, v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.text(s)),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.value(x)).collect()),
            Value::Object(o) => Value::Object(
                o.iter()
                    .map(|(k, v)| {
                        // Ids derived from the temporary corpus path and
                        // the indexing time vary between runs.
                        if matches!(k.as_str(), "build_id" | "source_id" | "record_id")
                            && v.is_string()
                        {
                            (k.clone(), Value::String(format!("<{}>", k.to_uppercase())))
                        } else {
                            (k.clone(), self.value(v))
                        }
                    })
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

fn words(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn ask_and_ai_commands_are_as_expected() {
    let golden_path = repo().join("fixtures/expected/ai_cli.json");
    let mut golden: Vec<Value> =
        serde_json::from_str(&std::fs::read_to_string(&golden_path).unwrap()).unwrap();
    // `RAGMONK_BLESS=1` rewrites the expected output from this build.
    let bless = std::env::var_os("RAGMONK_BLESS").is_some();
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let corpus = dir.path().join("source");
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for (from, to) in FIXTURES {
        copy(&repo().join(from), &corpus.join(to));
    }
    let (port, state) = mock();
    let base = |c: &mut Command| {
        c.env("RAGMONK_HOME", &home)
            .env("RAGMONK_SEARCH__SEMANTIC", "false")
            .env("RAGMONK_UPDATES__ENABLED", "false")
            .env("RAGMONK_AI__PROVIDER", "ollama")
            .env("RAGMONK_AI__BASE_URL", format!("http://127.0.0.1:{port}"))
            .env("PATH", &bin)
            .stdin(Stdio::null());
    };
    for args in [
        vec!["init"],
        vec!["source", "add", corpus.to_str().unwrap()],
        vec!["index"],
    ] {
        let mut c = Command::new(env!("CARGO_BIN_EXE_ragmonk"));
        base(&mut c);
        let o = c.args(&args).output().unwrap();
        assert!(o.status.success(), "{args:?}: {o:?}");
    }

    let mut subs: Vec<(String, String)> = Vec::new();
    for (p, label) in [(&corpus, "<CORPUS>"), (&home, "<HOME>")] {
        subs.push((p.to_string_lossy().replace('\\', "/"), label.into()));
        if let Ok(real) = std::fs::canonicalize(p) {
            let real = real.to_string_lossy().replace('\\', "/");
            subs.push((real.trim_start_matches("//?/").to_owned(), label.into()));
            subs.push((real, label.into()));
        }
    }
    subs.push((port.to_string(), "<PORT>".into()));
    subs.sort_by_key(|(n, _)| std::cmp::Reverse(n.len()));
    let norm = Norm { subs };

    let mut failures = Vec::new();
    for case in golden.iter_mut() {
        let name = case["case"].as_str().unwrap().to_owned();
        if let Some(r) = case["response"].as_object() {
            let mut s = state.lock().unwrap();
            s.0 = r["status"].as_u64().unwrap() as u16;
            s.1 = match &r["body"] {
                Value::String(t) => t.clone(),
                other => other.to_string(),
            };
        }
        state.lock().unwrap().2.clear();
        let mut c = Command::new(env!("CARGO_BIN_EXE_ragmonk"));
        base(&mut c);
        for (k, v) in case["env"].as_object().unwrap() {
            c.env(k, v.as_str().unwrap());
        }
        let args: Vec<&str> = case["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        let o = c.args(&args).output().unwrap();
        let stdout = String::from_utf8_lossy(&o.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&o.stderr).into_owned();
        if bless {
            case["exit_code"] = o.status.code().into();
            let json_out = serde_json::from_str::<Value>(case["stdout"].as_str().unwrap()).is_ok();
            case["stdout"] = match serde_json::from_str::<Value>(&stdout) {
                Ok(got) if json_out => serde_json::to_string(&norm.value(&got)).unwrap().into(),
                _ => norm.text(&stdout).into(),
            };
            case["stderr"] = norm.text(&stderr).into();
            case["sent"] = norm.value(&Value::Array(state.lock().unwrap().2.clone()));
            continue;
        }
        if o.status.code() != case["exit_code"].as_i64().map(|c| c as i32) {
            failures.push(format!(
                "{name}: exit {:?}, expected {}\n{stderr}",
                o.status.code(),
                case["exit_code"]
            ));
            continue;
        }
        let expected_out = case["stdout"].as_str().unwrap();
        let same_out = match serde_json::from_str::<Value>(expected_out) {
            Ok(expected) => serde_json::from_str::<Value>(&stdout)
                .map(|got| norm.value(&got) == expected)
                .unwrap_or(false),
            Err(_) => norm.text(&stdout) == expected_out,
        };
        if !same_out {
            failures.push(format!(
                "{name} stdout:\n--- got\n{}\n--- expected\n{expected_out}",
                norm.text(&stdout)
            ));
        }
        if words(&norm.text(&stderr)) != words(case["stderr"].as_str().unwrap()) {
            failures.push(format!(
                "{name} stderr:\n  got      {}\n  expected {}",
                norm.text(&stderr),
                case["stderr"]
            ));
        }
        let sent = norm.value(&Value::Array(state.lock().unwrap().2.clone()));
        if sent != case["sent"] {
            failures.push(format!(
                "{name} sent:\n  got      {sent}\n  expected {}",
                case["sent"]
            ));
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
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
