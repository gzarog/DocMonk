//! ChatGPT via the Codex App Server (beta): the official
//! `codex app-server`, spoken to over stdio JSON-RPC with its documented
//! protocol (`initialize`, `account/read`, `account/login/start`,
//! `model/list`, `thread/start`, `turn/start` and the turn notifications).
//!
//! Every answer runs in a fresh thread: approvals are `never`, the sandbox
//! is read-only, restricted to a private directory that holds nothing but
//! the turn's own images, and has no network. A request the runtime sends
//! back (an approval prompt) is refused by the transport. Only ChatGPT
//! sign-in is accepted; an API-key sign-in is refused and `OPENAI_API_KEY`
//! is never read. Failed, interrupted or empty turns are never presented as
//! answers.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use docmonk_config::model::AiConfig;
use docmonk_core::errors::DocMonkError;
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

/// The turn's sandbox: read-only, limited to `root`, no network.
fn sandbox_policy(root: &str) -> Value {
    json!({
        "type": "readOnly",
        "access": {"type": "restricted", "includePlatformDefaults": false, "readableRoots": [root]},
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

pub fn check_supported_version(v: Option<(u32, u32, u32)>) -> Result<(), DocMonkError> {
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
pub fn map_runtime_error(e: RpcError) -> DocMonkError {
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
    if is(&[
        "429",
        "quota_exceeded",
        "insufficient_quota",
        "usagelimitexceeded",
    ]) || text.contains("quota")
        || text.contains("usage limit")
        || text.contains("rate limit")
    {
        return quota_exhausted(format!(
            "the ChatGPT account's allowance is exhausted: {message}"
        ));
    }
    if is(&["401", "unauthenticated", "login_required"])
        || text.contains("login")
        || text.contains("auth")
    {
        return authentication_required(format!(
            "the Codex runtime needs sign-in: {message}. Run: docmonk ai login codex"
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

/// `account/read` as a status. A sign-in that is not ChatGPT (an API key)
/// is refused: the codex provider is the subscription path.
pub fn status_from_result(r: &Value) -> Result<RuntimeStatus, DocMonkError> {
    let account = &r["account"];
    let authenticated = account.is_object();
    let mode = &account["type"];
    if authenticated && mode != "chatgpt" {
        return Err(policy_blocked(format!(
            "the Codex runtime is signed in with an unexpected auth mode {}; DocMonk's codex \
             provider requires ChatGPT (subscription) sign-in, not an API key",
            quoted(mode)
        )));
    }
    Ok(RuntimeStatus {
        provider_id: PROVIDER_ID.into(),
        authenticated,
        account: account["email"].as_str().map(str::to_owned),
        runtime_version: None,
        detail: if authenticated {
            match account["planType"].as_str() {
                Some(plan) => format!("signed in (ChatGPT {plan})"),
                None => "signed in".into(),
            }
        } else {
            "not signed in; run 'docmonk ai login codex'".into()
        },
    })
}

fn int(v: &Value) -> Option<i64> {
    v.as_i64().filter(|_| !v.is_f64())
}

/// A failed turn's error as a semantic error.
fn turn_error(turn: &Value) -> DocMonkError {
    let message = turn["error"]["message"]
        .as_str()
        .unwrap_or("the turn failed")
        .to_owned();
    let info = plain(&turn["error"]["codexErrorInfo"]);
    let e = map_runtime_error(RpcError::Remote {
        code: if info.is_empty() {
            Value::Null
        } else {
            Value::String(info)
        },
        message: message.clone(),
    });
    if e.class() == Some("JsonRpcError") {
        crate::errors::provider_error(format!("the Codex turn failed: {message}"))
    } else {
        e
    }
}

/// What one turn produced, from its notifications.
#[derive(Debug, Default)]
pub struct TurnOutcome {
    pub text: String,
    pub usage: AiUsage,
}

/// The protocol operations over one connection; each answer is a fresh
/// thread with one turn.
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

    fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value, DocMonkError> {
        self.client
            .request(method, params, self.timeout)
            .map_err(map_runtime_error)
    }

    pub fn initialize(&mut self) -> Result<(), DocMonkError> {
        if self.initialized {
            return Ok(());
        }
        self.request(
            "initialize",
            Some(json!({
                "clientInfo": {
                    "name": "docmonk",
                    "title": "DocMonk",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            })),
        )?;
        self.client.notify("initialized", Some(json!({})))?;
        self.initialized = true;
        Ok(())
    }

    pub fn account_status(&mut self) -> Result<RuntimeStatus, DocMonkError> {
        self.initialize()?;
        let r = self.request("account/read", Some(json!({"refreshToken": false})))?;
        status_from_result(&r)
    }

    /// ChatGPT device-code sign-in: prints where to go and the code, waits
    /// for the runtime to report completion, then reads the account.
    pub fn login(&mut self) -> Result<RuntimeStatus, DocMonkError> {
        self.initialize()?;
        let r = self.request(
            "account/login/start",
            Some(json!({"type": "chatgptDeviceCode"})),
        )?;
        let login_id = r["loginId"].clone();
        if let (Some(url), Some(code)) = (r["verificationUrl"].as_str(), r["userCode"].as_str()) {
            eprintln!("Sign in to ChatGPT: open {url} and enter the code {code}");
        }
        // The person has a few minutes to finish in the browser.
        let wait = self.timeout.max(600.0);
        let deadline = Instant::now() + Duration::from_secs_f64(wait);
        loop {
            let left = deadline
                .saturating_duration_since(Instant::now())
                .as_secs_f64();
            let Some(n) = self
                .client
                .next_notification(left)
                .map_err(map_runtime_error)?
            else {
                return Err(authentication_required(
                    "ChatGPT sign-in did not complete in time; run 'docmonk ai login codex' again",
                ));
            };
            if n["method"] == "account/login/completed"
                && (login_id.is_null() || n["params"]["loginId"] == login_id)
            {
                if n["params"]["success"] != true {
                    return Err(authentication_required(format!(
                        "ChatGPT sign-in failed: {}",
                        plain(&n["params"]["error"])
                    )));
                }
                break;
            }
        }
        self.account_status()
    }

    pub fn logout(&mut self) -> Result<(), DocMonkError> {
        self.initialize()?;
        self.request("account/logout", None).map(|_| ())
    }

    pub fn models(&mut self) -> Result<Vec<String>, DocMonkError> {
        self.initialize()?;
        let r = self.request("model/list", Some(json!({})))?;
        Ok(r["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["hidden"] != true)
            .filter_map(|m| m["id"].as_str().or_else(|| m["model"].as_str()))
            .map(str::to_owned)
            .collect())
    }

    /// One turn in a fresh thread: `system` and `user` as text, then the
    /// local images, sandboxed to `root` (the images' directory). With
    /// `output_schema`, the final message must match it.
    pub fn run_turn(
        &mut self,
        system: &str,
        user: &str,
        images: &[std::path::PathBuf],
        root: &std::path::Path,
        model: &str,
        output_schema: Option<Value>,
    ) -> Result<TurnOutcome, DocMonkError> {
        self.initialize()?;
        let root_s = root.display().to_string();
        let mut thread_params = json!({
            "approvalPolicy": "never",
            "sandbox": "read-only",
            "cwd": root_s,
        });
        // Only a configured model is passed; otherwise the runtime picks.
        if !model.is_empty() {
            thread_params["model"] = json!(model);
        }
        let thread = self.request("thread/start", Some(thread_params))?;
        let thread_id = thread["thread"]["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid_response("Codex runtime did not return a thread id"))?
            .to_owned();
        let mut input = vec![json!({"type": "text", "text": format!("{system}\n\n{user}")})];
        input.extend(
            images
                .iter()
                .map(|p| json!({"type": "localImage", "path": p.display().to_string()})),
        );
        let mut params = json!({
            "threadId": thread_id,
            "input": input,
            "approvalPolicy": "never",
            "sandboxPolicy": sandbox_policy(&root_s),
        });
        if !model.is_empty() {
            params["model"] = json!(model);
        }
        if let Some(schema) = output_schema {
            params["outputSchema"] = schema;
        }
        let started = self.request("turn/start", Some(params))?;
        let turn_id = started["turn"]["id"].clone();
        let deadline = Instant::now()
            + Duration::try_from_secs_f64(self.timeout.max(0.0)).unwrap_or(Duration::MAX);
        let mut final_text: Option<String> = None;
        let mut last_text: Option<String> = None;
        let mut usage = AiUsage::default();
        loop {
            let left = deadline
                .saturating_duration_since(Instant::now())
                .as_secs_f64();
            let Some(n) = self
                .client
                .next_notification(left)
                .map_err(map_runtime_error)?
            else {
                return Err(crate::errors::timeout(format!(
                    "the Codex turn did not finish within {:.0}s",
                    self.timeout
                )));
            };
            let p = &n["params"];
            let ours =
                |turn: &Value| turn_id.is_null() || turn["id"].is_null() || turn["id"] == turn_id;
            match n["method"].as_str().unwrap_or_default() {
                "item/completed" if p["item"]["type"] == "agentMessage" => {
                    let text = p["item"]["text"].as_str().unwrap_or_default().to_owned();
                    if p["item"]["phase"] == "final_answer" {
                        final_text = Some(text);
                    } else {
                        last_text = Some(text);
                    }
                }
                "thread/tokenUsage/updated" => {
                    let u = &p["tokenUsage"]["total"];
                    usage = AiUsage {
                        input_tokens: int(&u["inputTokens"]),
                        output_tokens: int(&u["outputTokens"]),
                    };
                }
                "turn/completed" if ours(&p["turn"]) => {
                    let turn = &p["turn"];
                    match turn["status"].as_str() {
                        Some("completed") | None => {}
                        Some("failed") => return Err(turn_error(turn)),
                        Some(other) => {
                            return Err(invalid_response(format!(
                                "Codex turn did not complete (status={}); refusing to present \
                                 it as an answer",
                                quoted(&Value::String(other.to_owned()))
                            )))
                        }
                    }
                    let text = final_text.or(last_text).unwrap_or_default();
                    if text.trim().is_empty() {
                        return Err(invalid_response("Codex turn returned no answer text"));
                    }
                    return Ok(TurnOutcome { text, usage });
                }
                _ => {}
            }
        }
    }

    /// `docmonk ask`: a text-only turn.
    pub fn answer(
        &mut self,
        system: &str,
        user: &str,
        model: &str,
    ) -> Result<(String, String, AiUsage), DocMonkError> {
        let dir = crate::analysis_subscription::TempDir::new().map_err(|e| {
            crate::errors::provider_error(format!("cannot create a temporary directory: {e}"))
        })?;
        let out = self.run_turn(system, user, &[], dir.path(), model, None)?;
        let model = if model.is_empty() { PROVIDER_ID } else { model };
        Ok((out.text, model.to_owned(), out.usage))
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

fn verify_version() -> Result<(), DocMonkError> {
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

fn spawn(timeout: f64) -> Result<Spawned, DocMonkError> {
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

    fn session(&mut self) -> Result<&mut CodexSession, DocMonkError> {
        if self.spawned.is_none() {
            self.spawned = Some(spawn(self.timeout)?);
        }
        Ok(&mut self.spawned.as_mut().expect("spawned").session)
    }
}

impl CodexRuntime {
    /// One answer in a fresh, sandboxed thread.
    pub fn answer(
        &mut self,
        system: &str,
        user: &str,
        model: &str,
    ) -> Result<(String, String, AiUsage), DocMonkError> {
        self.session()?.answer(system, user, model)
    }

    /// One turn with local images and an optional output schema (file
    /// analysis); see [`CodexSession::run_turn`].
    pub fn run_turn(
        &mut self,
        system: &str,
        user: &str,
        images: &[std::path::PathBuf],
        root: &std::path::Path,
        model: &str,
        output_schema: Option<Value>,
    ) -> Result<TurnOutcome, DocMonkError> {
        self.session()?
            .run_turn(system, user, images, root, model, output_schema)
    }
}

impl SubscriptionRuntime for CodexRuntime {
    fn status(&mut self) -> Result<RuntimeStatus, DocMonkError> {
        self.session()?.account_status()
    }
    fn login(&mut self) -> Result<RuntimeStatus, DocMonkError> {
        self.session()?.login()
    }
    fn logout(&mut self) -> Result<(), DocMonkError> {
        self.session()?.logout()
    }
    fn models(&mut self) -> Result<Vec<String>, DocMonkError> {
        self.session()?.models()
    }
}

/// `docmonk ask` through a fresh, isolated Codex turn.
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
    fn answer(&mut self, request: &AiRequest) -> Result<AiAnswer, DocMonkError> {
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
