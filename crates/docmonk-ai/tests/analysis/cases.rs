//! File analysis providers (ADR 0045) against a local HTTP mock and a fake
//! `claude` CLI: the request each sends and how each answer is classed.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use docmonk_ai::analysis::{AiAnalyzer, AnalysisErrorKind, AnalysisInput, InputPart};
use docmonk_ai::analysis_http::{AnthropicAnalyzer, OpenAiAnalyzer};
use serde_json::{json, Value};

struct Seen {
    path: String,
    headers: Vec<(String, String)>,
    body: Value,
}

/// Serves one canned response; returns the base URL and what was received.
fn serve(status: u16, body: String) -> (String, Arc<Mutex<Option<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(None));
    let s2 = seen.clone();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let path = line
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_owned();
        let mut headers = Vec::new();
        let mut len = 0usize;
        loop {
            let mut h = String::new();
            reader.read_line(&mut h).unwrap();
            let h = h.trim_end().to_owned();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                let (k, v) = (k.trim().to_lowercase(), v.trim().to_owned());
                if k == "content-length" {
                    len = v.parse().unwrap();
                }
                headers.push((k, v));
            }
        }
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).unwrap();
        *s2.lock().unwrap() = Some(Seen {
            path,
            headers,
            body: serde_json::from_slice(&buf).unwrap_or(Value::Null),
        });
        let resp = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(resp.as_bytes()).unwrap();
    });
    (format!("http://127.0.0.1:{port}"), seen)
}

fn analysis() -> Value {
    json!({
        "schema_version": 1, "title": "Notice", "author": null, "language": "en",
        "summary": "A notice.", "keywords": ["notice"], "page_count": 1,
        "chunks": [{"kind": "text", "heading_path": [], "heading_level": null, "page_start": 1,
                    "page_end": 1, "text": "Retries run hourly.", "embedding_text": null,
                    "table_rows": null}],
        "code_references": [{"kind": "symbol", "value": "RetryService", "chunk_index": 0,
                             "quote": "Retries"}]
    })
}

fn pdf_input() -> AnalysisInput {
    AnalysisInput {
        file_name: "notice.pdf".into(),
        format: "pdf".into(),
        parts: vec![InputPart::Pdf {
            bytes: b"%PDF-1.4 fake".to_vec(),
        }],
        page_count: Some(1),
    }
}

fn header<'a>(s: &'a Seen, k: &str) -> Option<&'a str> {
    s.headers
        .iter()
        .find(|(h, _)| h == k)
        .map(|(_, v)| v.as_str())
}

#[test]
fn anthropic_sends_the_pdf_natively_with_structured_output() {
    let reply = json!({"model": "claude-opus-5-5", "stop_reason": "end_turn",
        "content": [{"type": "thinking", "thinking": ""}, {"type": "text", "text": analysis().to_string()}]});
    let (url, seen) = serve(200, reply.to_string());
    let a = AnthropicAnalyzer::new(
        "sk-test".into(),
        "claude-opus-5-5".into(),
        &url,
        10.0,
        16000,
    );
    let r = a.analyze(&pdf_input()).unwrap();
    assert_eq!(r.chunks[0].text, "Retries run hourly.");
    let s = seen.lock().unwrap().take().unwrap();
    assert_eq!(s.path, "/v1/messages");
    assert_eq!(header(&s, "x-api-key"), Some("sk-test"));
    assert_eq!(header(&s, "anthropic-version"), Some("2026-10-21"));
    assert_eq!(
        header(&s, "anthropic-beta"),
        Some("server-side-fallback-2026-07-01")
    );
    let b = &s.body;
    assert_eq!(b["model"], "claude-opus-5-5");
    assert_eq!(b["fallbacks"], "default");
    assert!(b.get("temperature").is_none() && b.get("tool_choice").is_none());
    assert_eq!(b["output_config"]["format"]["type"], "json_schema");
    let content = b["messages"][0]["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "document");
    assert_eq!(content[0]["source"]["media_type"], "application/pdf");
    assert_eq!(content[0]["source"]["data"], "JVBERi0xLjQgZmFrZQ==");
    assert_eq!(content.last().unwrap()["type"], "text");
}

#[test]
fn anthropic_failures_are_classed() {
    let cases = [
        (
            429,
            json!({"type": "error", "error": {"type": "rate_limit_error"}}),
            AnalysisErrorKind::Quota,
        ),
        (
            401,
            json!({"type": "error"}),
            AnalysisErrorKind::NotSignedIn,
        ),
        (
            529,
            json!({"type": "error", "error": {"type": "overloaded_error"}}),
            AnalysisErrorKind::Failed,
        ),
        (400, json!({"type": "error"}), AnalysisErrorKind::Rejected),
        (
            200,
            json!({"stop_reason": "refusal", "stop_details": {"category": "cyber"}, "content": []}),
            AnalysisErrorKind::Refused,
        ),
        (
            200,
            json!({"stop_reason": "max_tokens", "content": [{"type": "text", "text": "{"}]}),
            AnalysisErrorKind::InvalidOutput,
        ),
        (
            200,
            json!({"stop_reason": "end_turn", "content": [{"type": "text", "text": "{\"schema_version\": 1}"}]}),
            AnalysisErrorKind::InvalidOutput,
        ),
    ];
    for (status, body, want) in cases {
        let (url, _) = serve(status, body.to_string());
        let a = AnthropicAnalyzer::new("k".into(), "m".into(), &url, 10.0, 100);
        let e = a.analyze(&pdf_input()).unwrap_err();
        assert_eq!(e.kind, want, "{status} {body}: {}", e.message);
        assert_eq!(
            e.kind.transient(),
            matches!(
                want,
                AnalysisErrorKind::Quota
                    | AnalysisErrorKind::Failed
                    | AnalysisErrorKind::NotSignedIn
            )
        );
    }
    // Nothing listens: a transient connection failure.
    let a = AnthropicAnalyzer::new("k".into(), "m".into(), "http://127.0.0.1:9", 5.0, 100);
    assert_eq!(
        a.analyze(&pdf_input()).unwrap_err().kind,
        AnalysisErrorKind::Failed
    );
}

#[test]
fn openai_sends_input_file_with_a_strict_schema() {
    let reply = json!({"status": "completed", "model": "gpt-4o-mini",
        "output": [{"type": "message", "content": [{"type": "output_text", "text": analysis().to_string()}]}]});
    let (url, seen) = serve(200, reply.to_string());
    let a = OpenAiAnalyzer::new("sk-o".into(), "gpt-4o-mini".into(), &url, 10.0, 16000);
    let r = a.analyze(&pdf_input()).unwrap();
    assert_eq!(r.code_references[0].value, "RetryService");
    let s = seen.lock().unwrap().take().unwrap();
    assert_eq!(s.path, "/responses");
    assert_eq!(header(&s, "authorization"), Some("Bearer sk-o"));
    let b = &s.body;
    assert_eq!(b["text"]["format"]["strict"], true);
    assert_eq!(b["text"]["format"]["name"], "docmonk_analysis");
    let c = &b["input"][0]["content"][0];
    assert_eq!(c["type"], "input_file");
    assert_eq!(
        c["file_data"],
        "data:application/pdf;base64,JVBERi0xLjQgZmFrZQ=="
    );

    let refusal = json!({"status": "completed", "output": [{"type": "message", "content": [{"type": "refusal", "refusal": "no"}]}]});
    let (url, _) = serve(200, refusal.to_string());
    let a = OpenAiAnalyzer::new("k".into(), "m".into(), &url, 10.0, 100);
    assert_eq!(
        a.analyze(&pdf_input()).unwrap_err().kind,
        AnalysisErrorKind::Refused
    );
    let cut = json!({"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}, "output": []});
    let (url, _) = serve(200, cut.to_string());
    let a = OpenAiAnalyzer::new("k".into(), "m".into(), &url, 10.0, 100);
    assert_eq!(
        a.analyze(&pdf_input()).unwrap_err().kind,
        AnalysisErrorKind::InvalidOutput
    );
}

/// A fake `claude` that records its arguments, working directory and
/// environment, then prints `reply`.
#[cfg(unix)]
fn fake_claude(dir: &std::path::Path, reply: &Value, code: i32) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let log = dir.join("log");
    let reply_file = dir.join("reply.json");
    std::fs::write(&reply_file, reply.to_string()).unwrap();
    let script = format!(
        "#!/bin/sh\n{{ printf 'ARGS'; for a in \"$@\"; do printf ' [%s]' \"$a\"; done; echo; echo \"PWD $PWD\"; ls; env | grep -E 'API_KEY|AUTH_TOKEN|GH_TOKEN' ; }} > '{}' 2>&1\ncat '{}'\nexit {code}\n",
        log.display(),
        reply_file.display()
    );
    let bin = dir.join("claude");
    std::fs::write(&bin, script).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    log
}

#[cfg(unix)]
#[test]
fn claude_code_runs_the_official_cli_locked_down_and_without_api_keys() {
    use docmonk_ai::analysis_subscription::ClaudeCodeAnalyzer;
    let bin = tempfile::tempdir().unwrap();
    let reply = json!({"type": "result", "is_error": false, "result": "",
        "stop_reason": "end_turn", "structured_output": analysis()});
    let log = fake_claude(bin.path(), &reply, 0);
    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![bin.path().to_path_buf()];
    paths.extend(std::env::split_paths(&old_path));
    let mut a = ClaudeCodeAnalyzer::new("opus".into(), 30.0);
    a.cli.search_path = Some(std::env::join_paths(paths).unwrap());
    a.cli.env = vec![
        ("ANTHROPIC_API_KEY".into(), "sk-must-not-leak".into()),
        ("GH_TOKEN".into(), "ghp-must-not-leak".into()),
    ];
    let r = a.analyze(&pdf_input());
    let r = r.unwrap();
    assert_eq!(r.title.as_deref(), Some("Notice"));
    let log = std::fs::read_to_string(log).unwrap();
    assert!(!log.contains("must-not-leak"), "{log}");
    for want in [
        "[-p]",
        "[--restricted]",
        "[--tools] [Read]",
        "[--no-session-persistence]",
        "[--strict-mcp-config]",
        "[--permission-mode] [dontAsk]",
        "[--model] [opus]",
        "[--output-format] [json]",
        "part-0.pdf",
    ] {
        assert!(log.contains(want), "{want} missing:\n{log}");
    }
    // The original file name never reaches the file system.
    let listing: Vec<&str> = log
        .lines()
        .skip_while(|l| !l.starts_with("PWD "))
        .skip(1)
        .collect();
    assert_eq!(listing, ["part-0.pdf"], "{log}");
}

#[cfg(unix)]
#[test]
fn claude_code_usage_limit_is_a_transient_quota_error() {
    use docmonk_ai::analysis_subscription::{claude_result, CliOutput};
    let out = CliOutput {
        code: Some(1),
        stdout: json!({"is_error": true, "result": "Claude AI usage limit reached|1767323045"})
            .to_string(),
        stderr: String::new(),
    };
    let e = claude_result(&out, None).unwrap_err();
    assert_eq!(e.kind, AnalysisErrorKind::Quota);
    assert!(e.kind.transient());
}
