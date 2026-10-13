//! ChatGPT via the Codex App Server (beta): the official
//! `codex app-server` spoken to over stdio JSON-RPC.
//!
//! Every answer runs in a fresh, isolated thread/turn (tools, files, web,
//! plugins, hooks and inherited MCP servers disabled in the runtime
//! configuration). Only ChatGPT sign-in is accepted; `OPENAI_API_KEY` is
//! never read. Incomplete or empty turns are never presented as answers.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use docmonk_config::model::AiConfig;
use docmonk_core::errors::RagMonkError;
use serde_json::{json, Value};

use crate::errors::{
    authentication_required, invalid_response, policy_blocked, quota_exhausted,
    runtime_unavailable, unsupported_version,
};
use crate::prompt::{build_prompt, AiAnswer, AiRequest, AiUsage, SYSTEM_PROMPT};
use crate::runtime::{which, RuntimeStatus, SubscriptionRuntime};
use crate::text::{plain, quoted};
use crate::transport::{JsonRpcClient, RpcError};
use crate::AiProvider;

pub const PROVIDER_ID: &str = "codex";
/// A fixed executable name, never a path from (untrusted) config.
const EXECUTABLE: &str = "codex";
const MIN_TESTED_VERSION: (u32, u32, u32) = (0, 0, 0);

fn isolation() -> Value {
    json!({
        "tools": false,
        "fileAccess": false,
        "web": false,
        "plugins": false,
        "hooks": false,
        "mcpServers": [],
    })
}

/// The first `a.b[.c]` in a `--version` output.
pub fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let rest = &text[i..];
            let mut parts = rest.splitn(4, '.');
            let num = |s: Option<&str>| -> Option<u32> {
                let d: String = s?.chars().take_while(char::is_ascii_digit).collect();
                d.parse().ok()
            };
            let a = num(parts.next());
            let second = parts.next();
            if let (Some(a), Some(b)) = (a, num(second)) {
                let full_b = second.unwrap_or_default();
                let c = if full_b.chars().all(|c| c.is_ascii_digit()) {
                    num(parts.next()).unwrap_or(0)
                } else {
                    0
                };
                return Some((a, b, c));
            }
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    None
}

pub fn check_supported_version(v: Option<(u32, u32, u32)>) -> Result<(), RagMonkError> {
    match v {
        Some(v) if v < MIN_TESTED_VERSION => Err(unsupported_version(format!(
            "the installed {EXECUTABLE} runtime {}.{}.{} is older than the tested minimum {}.{}.{}; \
             upgrade it to use this provider",
            v.0, v.1, v.2, MIN_TESTED_VERSION.0, MIN_TESTED_VERSION.1, MIN_TESTED_VERSION.2
        ))),
        _ => Ok(()),
    }
}

/// The runtime's error reply as a semantic error (`_map_runtime_error`).
pub fn map_runtime_error(e: RpcError) -> RagMonkError {
    let RpcError::Remote { code, message } = e else {
        return e.into_error();
    };
    let code_s = if code.is_null() {
        String::new()
    } else {
        plain(&code).to_lowercase()
    };
    let text = format!("{code_s} {message}").to_lowercase();
    let is = |set: &[&str]| set.contains(&code_s.as_str());
    if is(&["429", "quota_exceeded", "insufficient_quota"]) || text.contains("quota") {
        return quota_exhausted(format!(
            "the ChatGPT account's allowance is exhausted: {message}"
        ));
    }
    if is(&["401", "unauthenticated", "login_required"])
        || text.contains("login")
        || text.contains("auth")
    {
        return authentication_required(format!(
            "the Codex runtime needs sign-in: {message}. Run: ragmonk ai login codex"
        ));
    }
    if is(&["403", "forbidden", "policy"]) || text.contains("policy") || text.contains("forbidden")
    {
        return policy_blocked(format!(
            "the Codex runtime refused the request on policy grounds: {message}"
        ));
    }
    RpcError::Remote { code, message }.into_error()
}

fn opt_str(v: &Value) -> Option<String> {
    v.as_str().map(str::to_owned)
}

pub fn status_from_result(r: &Value) -> Result<RuntimeStatus, RagMonkError> {
    let authenticated = truthy(&r["authenticated"]);
    let account = match &r["account"] {
        Value::Object(a) => ["email", "id", "name"]
            .iter()
            .map(|k| a.get(*k).unwrap_or(&Value::Null))
            .find(|v| truthy(v))
            .and_then(opt_str),
        Value::String(s) => Some(s.clone()),
        _ => None,
    };
    let mode = &r["authMode"];
    if authenticated && !mode.is_null() && mode != "chatgpt" {
        return Err(policy_blocked(format!(
            "the Codex runtime is signed in with an unexpected auth mode {}; RagMonk's codex \
             provider requires ChatGPT (subscription) sign-in, not an API key",
            quoted(mode)
        )));
    }
    Ok(RuntimeStatus {
        provider_id: PROVIDER_ID.into(),
        authenticated,
        account,
        runtime_version: opt_str(&r["runtimeVersion"]),
        detail: if authenticated {
            "signed in".into()
        } else {
            "not signed in; run 'ragmonk ai login codex'".into()
        },
    })
}

/// Truthiness of a JSON value: null, false, zero and empty strings,
/// arrays and objects are false.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn int(v: &Value) -> Option<i64> {
    v.as_i64().filter(|_| !v.is_f64())
}

pub fn answer_from_result(r: &Value) -> Result<(String, String, AiUsage), RagMonkError> {
    let status = &r["status"];
    if !status.is_null() && status != "completed" {
        return Err(invalid_response(format!(
            "Codex turn did not complete (status={}); refusing to present it as an answer",
            quoted(status)
        )));
    }
    let text = match &r["message"] {
        Value::Object(m) => match m.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter(|b| b.is_object() && b["type"] == "text")
                .map(|b| plain(b.get("text").unwrap_or(&Value::String(String::new()))))
                .collect(),
            _ => String::new(),
        },
        _ => r["text"].as_str().unwrap_or_default().to_owned(),
    };
    if text.is_empty() {
        return Err(invalid_response("Codex turn returned no answer text"));
    }
    let model = r["model"]
        .as_str()
        .filter(|m| !m.is_empty())
        .unwrap_or("codex")
        .to_owned();
    let usage = match &r["usage"] {
        Value::Object(u) => AiUsage {
            input_tokens: u.get("inputTokens").and_then(int),
            output_tokens: u.get("outputTokens").and_then(int),
        },
        _ => AiUsage::default(),
    };
    Ok((text, model, usage))
}

/// The protocol operations over one connection; each answer is a fresh
/// thread/turn.
pub struct CodexSession {
    client: JsonRpcClient,
    timeout: f64,
    initialized: bool,
}

impl CodexSession {
    pub fn new(client: JsonRpcClient, timeout: f64) -> Self {
        Self {
            client,
            timeout,
            initialized: false,
        }
    }

    fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value, RagMonkError> {
        self.client
            .request(method, params, self.timeout)
            .map_err(map_runtime_error)
    }

    pub fn initialize(&mut self) -> Result<(), RagMonkError> {
        if self.initialized {
            return Ok(());
        }
        self.request(
            "initialize",
            Some(json!({
                "clientInfo": {"name": "ragmonk", "title": "RagMonk"},
                "capabilities": {"isolation": isolation()},
                "authMode": "chatgpt",
            })),
        )?;
        self.client.notify("initialized", Some(json!({})))?;
        self.initialized = true;
        Ok(())
    }

    pub fn account_status(&mut self) -> Result<RuntimeStatus, RagMonkError> {
        self.initialize()?;
        let r = self.request("account/getStatus", None)?;
        status_from_result(&r)
    }

    pub fn login(&mut self) -> Result<RuntimeStatus, RagMonkError> {
        self.initialize()?;
        let r = self.request("account/login", Some(json!({"mode": "chatgpt"})))?;
        status_from_result(&r)
    }

    pub fn logout(&mut self) -> Result<(), RagMonkError> {
        self.initialize()?;
        self.request("account/logout", None).map(|_| ())
    }

    pub fn models(&mut self) -> Result<Vec<String>, RagMonkError> {
        self.initialize()?;
        let r = self.request("model/list", None)?;
        Ok(r["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| match e {
                Value::Object(o) => ["id", "name"]
                    .iter()
                    .map(|k| o.get(*k).unwrap_or(&Value::Null))
                    .find(|v| truthy(v))
                    .and_then(opt_str),
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect())
    }

    pub fn answer(
        &mut self,
        system: &str,
        user: &str,
        model: &str,
    ) -> Result<(String, String, AiUsage), RagMonkError> {
        self.initialize()?;
        let thread = self.request("thread/create", Some(json!({"isolation": isolation()})))?;
        let thread_id = [&thread["threadId"], &thread["id"]]
            .into_iter()
            .find(|v| truthy(v))
            .and_then(|v| v.as_str().map(str::to_owned))
            .ok_or_else(|| invalid_response("Codex runtime did not return a thread id"))?;
        let mut params = json!({
            "threadId": thread_id,
            "stream": false,
            "isolation": isolation(),
            "input": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
        });
        // Only a configured model is passed; otherwise the runtime picks.
        if !model.is_empty() {
            params["model"] = json!(model);
        }
        let r = self.request("thread/runTurn", Some(params))?;
        answer_from_result(&r)
    }

    pub fn close(&mut self) {
        self.client.close();
    }
}

/// A spawned `codex app-server`, torn down on drop.
struct Spawned {
    session: CodexSession,
    child: Child,
}

impl Drop for Spawned {
    fn drop(&mut self) {
        self.session.close();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn verify_version() -> Result<(), RagMonkError> {
    let Ok(mut child) = Command::new(EXECUTABLE)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Ok(()); // best effort; a real call still fails clearly
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(());
            }
        }
    }
    let mut out = String::new();
    if let Some(mut s) = child.stdout.take() {
        let _ = s.read_to_string(&mut out);
    }
    check_supported_version(parse_version(&out))
}

fn spawn(timeout: f64) -> Result<Spawned, RagMonkError> {
    if which(EXECUTABLE).is_none() {
        return Err(runtime_unavailable(format!(
            "the '{EXECUTABLE}' runtime is not installed or not on PATH; install it to use the \
             codex provider (see docs/providers)."
        )));
    }
    verify_version()?;
    let mut child = Command::new(EXECUTABLE)
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            runtime_unavailable(format!("could not start the '{EXECUTABLE}' runtime: {e}"))
        })?;
    // Drain stderr so diagnostics never block or corrupt the channel.
    if let Some(mut err) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut sink = [0u8; 65536];
            while matches!(err.read(&mut sink), Ok(n) if n > 0) {}
        });
    }
    let stdout = child.stdout.take().expect("piped stdout");
    let stdin = child.stdin.take().expect("piped stdin");
    let client = JsonRpcClient::new(Box::new(stdout), Box::new(stdin));
    Ok(Spawned {
        session: CodexSession::new(client, timeout),
        child,
    })
}

/// The lifecycle surface over a lazily spawned app server.
pub struct CodexRuntime {
    timeout: f64,
    spawned: Option<Spawned>,
}

impl CodexRuntime {
    pub fn new(ai: &AiConfig) -> Self {
        Self {
            timeout: ai.timeout_seconds,
            spawned: None,
        }
    }

    fn session(&mut self) -> Result<&mut CodexSession, RagMonkError> {
        if self.spawned.is_none() {
            self.spawned = Some(spawn(self.timeout)?);
        }
        Ok(&mut self.spawned.as_mut().expect("spawned").session)
    }
}

impl SubscriptionRuntime for CodexRuntime {
    fn status(&mut self) -> Result<RuntimeStatus, RagMonkError> {
        self.session()?.account_status()
    }
    fn login(&mut self) -> Result<RuntimeStatus, RagMonkError> {
        self.session()?.login()
    }
    fn logout(&mut self) -> Result<(), RagMonkError> {
        self.session()?.logout()
    }
    fn models(&mut self) -> Result<Vec<String>, RagMonkError> {
        self.session()?.models()
    }
}

/// `ragmonk ask` through a fresh, isolated Codex turn.
pub struct CodexProvider {
    runtime: CodexRuntime,
    model: String,
}

impl CodexProvider {
    pub fn new(ai: &AiConfig) -> Self {
        Self {
            runtime: CodexRuntime::new(ai),
            model: ai.model.clone(),
        }
    }
}

impl AiProvider for CodexProvider {
    fn answer(&mut self, request: &AiRequest) -> Result<AiAnswer, RagMonkError> {
        let model = self.model.clone();
        let (text, model, usage) =
            self.runtime
                .session()?
                .answer(SYSTEM_PROMPT, &build_prompt(request), &model)?;
        Ok(AiAnswer {
            text,
            provider: PROVIDER_ID.into(),
            model,
            usage,
        })
    }
}
