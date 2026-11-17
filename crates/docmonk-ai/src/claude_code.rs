//! Claude through a Claude Pro/Max subscription, via the official
//! `claude` CLI (beta), for `docmonk ask` and the `docmonk ai` lifecycle.
//!
//! The CLI runs headless (`claude -p`) with no tools at all, no settings,
//! hooks or MCP servers, and no saved session, so it can only answer from
//! the evidence DocMonk hands it. API-key variables are removed from its
//! environment (see [`CLAUDE_CONFLICTING_ENV`]), so the request always uses
//! the CLI's own sign-in. Sign-in and sign-out stay with the CLI itself.

use docmonk_config::model::AiConfig;
use docmonk_core::errors::DocMonkError;

use crate::analysis::AnalysisError;
use crate::analysis::AnalysisErrorKind;
pub use crate::analysis_subscription::CLAUDE_CONFLICTING_ENV;
use crate::analysis_subscription::{run_claude, CLAUDE_CODE_PROVIDER_ID, CLAUDE_SIGN_IN_HINT};
use crate::errors::{
    authentication_required, invalid_response, policy_blocked, provider_error, quota_exhausted,
    runtime_unavailable, timeout,
};
use crate::prompt::{build_prompt, AiAnswer, AiRequest, AiUsage, SYSTEM_PROMPT};
use crate::runtime::{RuntimeStatus, SubscriptionRuntime};
use crate::AiProvider;

pub const PROVIDER_ID: &str = CLAUDE_CODE_PROVIDER_ID;

fn to_error(e: AnalysisError) -> DocMonkError {
    match e.kind {
        AnalysisErrorKind::Quota => quota_exhausted(format!(
            "the Claude account's usage limit is reached: {}",
            e.message
        )),
        AnalysisErrorKind::NotSignedIn => authentication_required(e.message),
        AnalysisErrorKind::Timeout => timeout(e.message),
        AnalysisErrorKind::Refused => policy_blocked(e.message),
        AnalysisErrorKind::InvalidOutput => invalid_response(e.message),
        _ => provider_error(e.message),
    }
}

fn ensure_installed() -> Result<(), DocMonkError> {
    if crate::runtime::which(crate::analysis_subscription::CLAUDE_EXECUTABLE).is_none() {
        return Err(runtime_unavailable(
            "the Claude CLI ('claude') is not installed or not on PATH; install Claude Code and \
             sign in with it to use the claude_code provider (see docs/providers).",
        ));
    }
    Ok(())
}

pub struct ClaudeCodeRuntime {
    timeout: f64,
}

impl ClaudeCodeRuntime {
    pub fn new(ai: &AiConfig) -> Self {
        Self {
            timeout: ai.timeout_seconds,
        }
    }

    fn version(&self) -> Result<Option<String>, DocMonkError> {
        ensure_installed()?;
        let o = run_claude(
            &["--version".to_owned()],
            None,
            self.timeout.min(10.0),
            &Default::default(),
        )
        .map_err(to_error)?;
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

    /// One headless answer with every tool disabled.
    pub fn complete(
        &self,
        system: &str,
        prompt: &str,
        model: &str,
    ) -> Result<(String, String, AiUsage), DocMonkError> {
        ensure_installed()?;
        let mut args: Vec<String> = [
            "-p",
            prompt,
            "--output-format",
            "json",
            "--no-session-persistence",
            "--restricted",
            "--strict-mcp-config",
            "--tools",
            "",
            "--append-system-prompt",
            system,
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        if !model.is_empty() {
            args.push("--model".into());
            args.push(model.to_owned());
        }
        let out = run_claude(&args, None, self.timeout, &Default::default()).map_err(to_error)?;
        let v: serde_json::Value = serde_json::from_str(out.stdout.trim()).unwrap_or_default();
        if out.code != Some(0) || v["is_error"] == true || !v.is_object() {
            let detail = v["result"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{}{}", out.stdout.trim(), out.stderr.trim()));
            let kind = crate::analysis_subscription::classify_claude_failure(&detail);
            return Err(to_error(AnalysisError::new(kind, detail)));
        }
        if v["stop_reason"] == "refusal" {
            return Err(policy_blocked("Claude declined to answer"));
        }
        let text = v["result"].as_str().unwrap_or_default().trim().to_owned();
        if text.is_empty() {
            return Err(invalid_response("the Claude CLI returned no answer text"));
        }
        let usage = &v["usage"];
        let served = v["modelUsage"]
            .as_object()
            .and_then(|m| m.keys().next().cloned())
            .unwrap_or_else(|| {
                if model.is_empty() {
                    PROVIDER_ID.to_owned()
                } else {
                    model.to_owned()
                }
            });
        Ok((
            text,
            served,
            AiUsage {
                input_tokens: usage["input_tokens"].as_i64(),
                output_tokens: usage["output_tokens"].as_i64(),
            },
        ))
    }
}

impl SubscriptionRuntime for ClaudeCodeRuntime {
    fn status(&mut self) -> Result<RuntimeStatus, DocMonkError> {
        let version = self.version()?;
        Ok(RuntimeStatus {
            provider_id: PROVIDER_ID.into(),
            // The CLI has no sign-in query; a call reports it.
            authenticated: false,
            account: None,
            runtime_version: version,
            detail: format!("sign-in is managed by the Claude CLI; {CLAUDE_SIGN_IN_HINT}"),
        })
    }

    fn login(&mut self) -> Result<RuntimeStatus, DocMonkError> {
        self.version()?;
        Err(authentication_required(format!(
            "DocMonk does not sign in to Claude for you; {CLAUDE_SIGN_IN_HINT}"
        )))
    }

    fn logout(&mut self) -> Result<(), DocMonkError> {
        self.version()?;
        Err(authentication_required(
            "DocMonk does not sign out of Claude for you; run `claude` and use /logout",
        ))
    }

    fn models(&mut self) -> Result<Vec<String>, DocMonkError> {
        self.version()?;
        // The CLI takes aliases (`opus`, `sonnet`, `fable`) or full model ids.
        Ok(vec!["opus".into(), "sonnet".into(), "fable".into()])
    }
}

/// `docmonk ask` through `claude -p`.
pub struct ClaudeCodeProvider {
    runtime: ClaudeCodeRuntime,
    model: String,
}

impl ClaudeCodeProvider {
    pub fn new(ai: &AiConfig) -> Self {
        Self {
            runtime: ClaudeCodeRuntime::new(ai),
            model: ai.model.clone(),
        }
    }
}

impl AiProvider for ClaudeCodeProvider {
    fn answer(&mut self, request: &AiRequest) -> Result<AiAnswer, DocMonkError> {
        let (text, model, usage) =
            self.runtime
                .complete(SYSTEM_PROMPT, &build_prompt(request), &self.model)?;
        Ok(AiAnswer {
            text,
            provider: PROVIDER_ID.into(),
            model,
            usage,
        })
    }
}
