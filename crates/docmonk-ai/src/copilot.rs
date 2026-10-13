//! GitHub Copilot through the signed-in Copilot CLI (beta).
//!
//! Copilot has no Rust SDK, so RagMonk runs the official `copilot` CLI in
//! non-interactive mode (`copilot -p PROMPT`), which uses the CLI's own
//! sign-in. Token variables (`GH_TOKEN`, `GITHUB_TOKEN`, ...) are removed
//! from the child's environment so a token can never stand in for the
//! signed-in user, and no tool is approved, so the CLI cannot act on
//! retrieved evidence. Sign-in and sign-out stay with the CLI itself.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use docmonk_config::model::AiConfig;
use docmonk_core::errors::RagMonkError;

use crate::errors::{
    authentication_required, invalid_response, policy_blocked, quota_exhausted,
    runtime_unavailable, timeout,
};
use crate::prompt::{build_prompt, AiAnswer, AiRequest, AiUsage, SYSTEM_PROMPT};
use crate::runtime::{which, RuntimeStatus, SubscriptionRuntime};
use crate::AiProvider;

pub const PROVIDER_ID: &str = "github_copilot";
/// A fixed executable name, never a path from (untrusted) config.
const EXECUTABLE: &str = "copilot";
/// Credential variables that must not replace the CLI sign-in.
pub const CONFLICTING_ENV: &[&str] = &[
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "COPILOT_GITHUB_TOKEN",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
];
const SIGN_IN_HINT: &str = "sign in with the Copilot CLI itself: run `copilot` and use /login";

/// A CLI failure as a semantic error (`_map_client_error`).
pub fn map_client_error(text: &str) -> RagMonkError {
    let lower = text.to_lowercase();
    let has = |s: &str| lower.contains(s);
    if has("quota") || has("rate limit") || has("429") {
        return quota_exhausted(format!(
            "the Copilot account's allowance is exhausted: {text}"
        ));
    }
    if has("unauth") || has("login") || has("sign in") || has("401") {
        return authentication_required(format!(
            "the Copilot CLI needs sign-in: {text}. Then {SIGN_IN_HINT}"
        ));
    }
    if has("forbidden") || has("policy") || has("organization") || has("403") {
        return policy_blocked(format!(
            "Copilot refused the request on policy grounds: {text}"
        ));
    }
    invalid_response(format!(
        "the Copilot CLI returned an unusable result: {text}"
    ))
}

struct Output {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run(args: &[&str], timeout_s: f64) -> Result<Output, RagMonkError> {
    if which(EXECUTABLE).is_none() {
        return Err(runtime_unavailable(
            "the GitHub Copilot CLI ('copilot') is not installed or not on PATH; install it and \
             sign in with it to use the github_copilot provider (see docs/providers).",
        ));
    }
    let mut cmd = Command::new(EXECUTABLE);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in CONFLICTING_ENV {
        cmd.env_remove(name);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| runtime_unavailable(format!("could not start the '{EXECUTABLE}' CLI: {e}")))?;
    let mut out_pipe = child.stdout.take().expect("piped stdout");
    let mut err_pipe = child.stderr.take().expect("piped stderr");
    let out = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out_pipe.read_to_string(&mut s);
        s
    });
    let err = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err_pipe.read_to_string(&mut s);
        s
    });
    let deadline =
        Instant::now() + Duration::try_from_secs_f64(timeout_s.max(0.0)).unwrap_or(Duration::MAX);
    let status = loop {
        if let Some(s) = child.try_wait().ok().flatten() {
            break s;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(timeout(format!(
                "ai runtime did not respond within {timeout_s:.0}s; the request was cancelled"
            )));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Ok(Output {
        code: status.code(),
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

/// The CLI's version line, if it answers `--version`.
fn version(timeout_s: f64) -> Result<Option<String>, RagMonkError> {
    let o = run(&["--version"], timeout_s.min(10.0))?;
    Ok((o.code == Some(0))
        .then(|| {
            o.stdout
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_owned()
        })
        .filter(|v| !v.is_empty()))
}

pub struct CopilotRuntime {
    timeout: f64,
}

impl CopilotRuntime {
    pub fn new(ai: &AiConfig) -> Self {
        Self {
            timeout: ai.timeout_seconds,
        }
    }

    fn complete(
        &self,
        prompt: &str,
        model: &str,
    ) -> Result<(String, String, AiUsage), RagMonkError> {
        let mut args = vec!["-p", prompt];
        if !model.is_empty() {
            args.extend(["--model", model]);
        }
        let o = run(&args, self.timeout)?;
        if o.code != Some(0) {
            let detail = if o.stderr.trim().is_empty() {
                o.stdout.trim()
            } else {
                o.stderr.trim()
            };
            return Err(map_client_error(detail));
        }
        let text = o.stdout.trim().to_owned();
        if text.is_empty() {
            return Err(invalid_response(
                "Copilot completion returned no answer text",
            ));
        }
        let model = if model.is_empty() { PROVIDER_ID } else { model };
        // The CLI reports no token usage; none is invented.
        Ok((text, model.to_owned(), AiUsage::default()))
    }
}

impl SubscriptionRuntime for CopilotRuntime {
    fn status(&mut self) -> Result<RuntimeStatus, RagMonkError> {
        let version = version(self.timeout)?;
        Ok(RuntimeStatus {
            provider_id: PROVIDER_ID.into(),
            // The CLI exposes no sign-in query; a call reports it.
            authenticated: false,
            account: None,
            runtime_version: version,
            detail: format!("sign-in is managed by the Copilot CLI; {SIGN_IN_HINT}"),
        })
    }

    fn login(&mut self) -> Result<RuntimeStatus, RagMonkError> {
        version(self.timeout)?;
        Err(authentication_required(format!(
            "RagMonk does not sign in to Copilot for you; {SIGN_IN_HINT}"
        )))
    }

    fn logout(&mut self) -> Result<(), RagMonkError> {
        version(self.timeout)?;
        Err(authentication_required(
            "RagMonk does not sign out of Copilot for you; run `copilot` and use /logout",
        ))
    }

    fn models(&mut self) -> Result<Vec<String>, RagMonkError> {
        version(self.timeout)?;
        // The CLI lists its models interactively only.
        Ok(Vec::new())
    }
}

/// `ragmonk ask` through `copilot -p`.
pub struct CopilotProvider {
    runtime: CopilotRuntime,
    model: String,
}

impl CopilotProvider {
    pub fn new(ai: &AiConfig) -> Self {
        Self {
            runtime: CopilotRuntime::new(ai),
            model: ai.model.clone(),
        }
    }
}

impl AiProvider for CopilotProvider {
    fn answer(&mut self, request: &AiRequest) -> Result<AiAnswer, RagMonkError> {
        let prompt = format!("{SYSTEM_PROMPT}\n\n{}", build_prompt(request));
        let (text, model, usage) = self.runtime.complete(&prompt, &self.model)?;
        Ok(AiAnswer {
            text,
            provider: PROVIDER_ID.into(),
            model,
            usage,
        })
    }
}
