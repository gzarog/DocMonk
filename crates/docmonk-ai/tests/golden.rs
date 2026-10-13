//! The AI layer against its expected results (`fixtures/expected/ai.json`):
//! the evidence prompt, every HTTP provider's request and response/failure
//! mapping against a mock server, the factory's configuration/privacy
//! errors, and the Codex JSON-RPC exchanges against a scripted runtime.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use docmonk_ai::codex::CodexSession;
use docmonk_ai::factory::create_provider_with_env;
use docmonk_ai::prompt::{build_prompt, AiRequest};
use docmonk_ai::transport::JsonRpcClient;
use docmonk_config::model::{AiConfig, PrivacyConfig};
use docmonk_core::errors::DocMonkError;
use serde_json::{json, Value};

const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/expected/ai.json"
);

fn golden() -> Value {
    serde_json::from_str(&std::fs::read_to_string(GOLDEN).unwrap()).unwrap()
}

/// `DOCMONK_BLESS=1` rewrites expected results from this build's output.
fn bless() -> bool {
    std::env::var_os("DOCMONK_BLESS").is_some()
}

fn write_section(section: &str, cases: Vec<Value>) {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap();
    let mut g = golden();
    g[section] = Value::Array(cases);
    let mut out = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(b" ");
    serde::Serialize::serialize(
        &g,
        &mut serde_json::Serializer::with_formatter(&mut out, fmt),
    )
    .unwrap();
    out.push(b'\n');
    std::fs::write(GOLDEN, out).unwrap();
}

/// Stores `got` (`{"error": ..}` or `{key: ..}`) as the case's expectation.
fn record(case: &mut Value, got: &Value, key: &str, port: Option<u16>) {
    let obj = case.as_object_mut().unwrap();
    obj.remove("error");
    obj.remove(key);
    if let Some(e) = got.get("error") {
        let mut e = e.clone();
        if let (Some(port), Some(m)) = (port, e["message"].as_str()) {
            e["message"] = m.replace(&port.to_string(), "<PORT>").into();
        }
        obj.insert("error".into(), e);
    } else if let Some(v) = got.get(key) {
        obj.insert(key.into(), v.clone());
    }
}

fn request(v: &Value) -> AiRequest {
    AiRequest {
        question: v["question"].as_str().unwrap().into(),
        summary: v["summary"].as_str().unwrap().into(),
        evidence: v["evidence"].as_array().unwrap().clone(),
        graph_paths: v["graph_paths"].as_array().unwrap().clone(),
    }
}

fn error_json(e: &DocMonkError) -> Value {
    json!({
        "type": e.class().unwrap_or("DocMonkError"),
        "exit_code": e.exit_code(),
        "message": e.message(),
    })
}

fn ai_config(c: &Value) -> AiConfig {
    let mut ai = AiConfig::default();
    if let Some(p) = c["provider"].as_str() {
        ai.provider = p.into();
    }
    if let Some(m) = c["model"].as_str() {
        ai.model = m.into();
    }
    ai.base_url = c["base_url"].as_str().map(str::to_owned);
    ai
}

#[test]
fn prompt_is_as_expected() {
    let g = golden();
    assert_eq!(build_prompt(&request(&g["requests"]["full"])), g["prompt"]);
    assert_eq!(
        build_prompt(&request(&g["requests"]["empty"])),
        g["prompt_empty"]
    );
}

#[test]
fn factory_errors_are_as_expected() {
    let g = golden();
    let mut blessed = Vec::new();
    for case in g["factory"].as_array().unwrap() {
        let ai = ai_config(&case["config"]);
        let privacy = PrivacyConfig {
            external_ai_allowed: case["external_ai_allowed"].as_bool().unwrap(),
        };
        let all = case["env"] == "all";
        let env = move |k: &str| {
            (all && matches!(k, "OPENAI_API_KEY" | "ANTHROPIC_API_KEY")).then(|| "k".to_owned())
        };
        let got = match create_provider_with_env(&ai, &privacy, &env) {
            Ok(_) => json!({"ok": true}),
            Err(e) => json!({"error": error_json(&e)}),
        };
        let expected = case
            .get("error")
            .map(|e| json!({"error": e}))
            .unwrap_or(json!({"ok": true}));
        if bless() {
            let mut case = case.clone();
            record(&mut case, &got, "ok", None);
            case.as_object_mut().unwrap().remove("ok");
            blessed.push(case);
            continue;
        }
        assert_eq!(got, expected, "{case}");
    }
    if bless() {
        write_section("factory", blessed);
    }
}

struct Captured {
    path: String,
    headers: Vec<(String, String)>,
    body: Value,
}

/// A one-request-per-connection HTTP mock answering with `response`.
fn mock(response: (u16, String, String)) -> (u16, Arc<Mutex<Vec<Captured>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let path = line
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_owned();
            let mut headers = Vec::new();
            let mut length = 0usize;
            loop {
                let mut h = String::new();
                reader.read_line(&mut h).unwrap();
                let h = h.trim_end();
                if h.is_empty() {
                    break;
                }
                let (k, v) = h.split_once(':').unwrap();
                let (k, v) = (k.trim().to_lowercase(), v.trim().to_owned());
                if k == "content-length" {
                    length = v.parse().unwrap();
                }
                headers.push((k, v));
            }
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).unwrap();
            sink.lock().unwrap().push(Captured {
                path,
                headers,
                body: serde_json::from_slice(&body).unwrap_or(Value::Null),
            });
            let (status, ctype, text) = &response;
            let reply = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                text.len()
            );
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    (port, seen)
}

/// A port nothing listens on. An ephemeral port that was bound and
/// released can be taken by a concurrent test's mock server, turning a
/// "connection refused" case into a live connection.
const DEAD_PORT: u16 = 1;

#[test]
fn http_providers_behave_as_expected() {
    let g = golden();
    let env = |k: &str| match k {
        "OPENAI_API_KEY" => Some("sk-test".to_owned()),
        "ANTHROPIC_API_KEY" => Some("ak-test".to_owned()),
        "DOCMONK_AI_API_KEY" => Some("ck".to_owned()),
        _ => None,
    };
    let privacy = PrivacyConfig {
        external_ai_allowed: true,
    };
    let mut failures = Vec::new();
    let mut blessed = Vec::new();
    for case in g["providers"].as_array().unwrap() {
        let name = case["case"].as_str().unwrap();
        let req = request(&g["requests"][case["request"].as_str().unwrap()]);
        let (port, seen) = match case.get("response") {
            Some(r) => mock((
                r["status"].as_u64().unwrap() as u16,
                r["content_type"].as_str().unwrap().into(),
                r["body"].as_str().unwrap().into(),
            )),
            None => (DEAD_PORT, Arc::default()),
        };
        let cfg_text = case["config"]
            .to_string()
            .replace("<PORT>", &port.to_string())
            .replace("<DEAD>", &port.to_string());
        let ai = ai_config(&serde_json::from_str(&cfg_text).unwrap());
        let mut provider = create_provider_with_env(&ai, &privacy, &env).unwrap();
        let got = match provider.answer(&req) {
            Ok(a) => json!({"answer": a.to_json()}),
            Err(e) => json!({"error": error_json(&e)}),
        };
        let expected = match case.get("answer") {
            Some(a) => json!({"answer": a}),
            None => {
                let mut e = case["error"].clone();
                e["message"] = e["message"]
                    .as_str()
                    .unwrap()
                    .replace("<PORT>", &port.to_string())
                    .into();
                json!({"error": e})
            }
        };
        // The OS error text behind a refused Ollama connection is the
        // platform's; only the prefix is a contract.
        if name == "ollama/connection_refused" {
            let msg = got["error"]["message"].as_str().unwrap_or_default();
            if !msg.starts_with("ollama request failed: ") || got["error"]["exit_code"] != 1 {
                failures.push(format!("{name}: {got}"));
            }
        } else if bless() {
            let mut case = case.clone();
            let live = (port != DEAD_PORT).then_some(port);
            record(&mut case, &got, "answer", live);
            blessed.push(case);
            continue;
        } else if got != expected {
            failures.push(format!("{name}:\n  got      {got}\n  expected {expected}"));
        }
        if bless() {
            blessed.push(case.clone());
            continue;
        }
        if let Some(sent) = case.get("sent").filter(|s| !s.is_null()) {
            let seen = seen.lock().unwrap();
            let Some(c) = seen.first() else {
                failures.push(format!("{name}: nothing sent"));
                continue;
            };
            if c.path != sent["path"] || c.body != sent["body"] {
                failures.push(format!(
                    "{name}: sent {} {}\n  expected {} {}",
                    c.path, c.body, sent["path"], sent["body"]
                ));
            }
            for (k, v) in sent["headers"].as_object().unwrap() {
                let got = c
                    .headers
                    .iter()
                    .find(|(h, _)| h == k)
                    .map(|(_, v)| v.as_str());
                if got != v.as_str() {
                    failures.push(format!("{name}: header {k} = {got:?}, expected {v}"));
                }
            }
        }
    }
    if bless() {
        write_section("providers", blessed);
        return;
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A scripted runtime over OS pipes: answers each request with the next
/// reply, with a notification and an unknown-id reply first and the real
/// reply split across two writes (as the generator's stream does).
fn scripted(replies: Vec<Value>) -> (JsonRpcClient, std::thread::JoinHandle<Vec<Value>>) {
    let (to_client_r, mut to_client_w) = std::io::pipe().unwrap();
    let (from_client_r, from_client_w) = std::io::pipe().unwrap();
    let handle = std::thread::spawn(move || {
        let mut sent = Vec::new();
        let mut replies = replies.into_iter();
        for line in BufReader::new(from_client_r).lines() {
            let Ok(line) = line else { break };
            let message: Value = serde_json::from_str(&line).unwrap();
            let id = message.get("id").cloned();
            sent.push(message);
            if let Some(id) = id {
                let mut out = replies.next().unwrap_or(json!({"result": {}}));
                out["jsonrpc"] = json!("2.0");
                out["id"] = id;
                let raw = format!("{out}\n");
                let _ = to_client_w.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"progress\"}\n");
                let _ = to_client_w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":999,\"result\":{}}\n");
                let _ = to_client_w.write_all(&raw.as_bytes()[..7]);
                let _ = to_client_w.flush();
                let _ = to_client_w.write_all(&raw.as_bytes()[7..]);
                let _ = to_client_w.flush();
            }
        }
        sent
    });
    (
        JsonRpcClient::new(Box::new(to_client_r), Box::new(from_client_w)),
        handle,
    )
}

#[test]
fn codex_exchanges_are_as_expected() {
    let g = golden();
    let mut failures = Vec::new();
    let mut blessed = Vec::new();
    for case in g["codex"].as_array().unwrap() {
        let name = case["case"].as_str().unwrap();
        let (client, handle) = scripted(case["replies"].as_array().unwrap().clone());
        let mut session = CodexSession::new(client, 5.0);
        let result = match case["op"].as_str().unwrap() {
            "status" => session.account_status().map(|s| s.to_json()),
            "login" => session.login().map(|s| s.to_json()),
            "logout" => session.logout().map(|()| Value::Null),
            "models" => session.models().map(|m| json!(m)),
            _ => session
                .answer("SYS", "USER", case["model"].as_str().unwrap())
                .map(|(text, model, usage)| {
                    json!({
                        "text": text,
                        "model": model,
                        "usage": [usage.input_tokens, usage.output_tokens],
                    })
                }),
        };
        drop(session);
        let sent = handle.join().unwrap();
        let got = match result {
            Ok(r) => json!({"result": r}),
            Err(e) => json!({"error": error_json(&e)}),
        };
        let expected = match case.get("error") {
            Some(e) => json!({"error": e}),
            None => json!({"result": case["result"]}),
        };
        if bless() {
            let mut case = case.clone();
            record(&mut case, &got, "result", None);
            blessed.push(case);
            continue;
        }
        if got != expected {
            failures.push(format!("{name}:\n  got      {got}\n  expected {expected}"));
        }
        if json!(sent) != case["sent"] {
            failures.push(format!(
                "{name} sent:\n  got      {}\n  expected {}",
                json!(sent),
                case["sent"]
            ));
        }
    }
    if bless() {
        write_section("codex", blessed);
        return;
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
