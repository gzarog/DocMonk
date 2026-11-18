//! The subscription analysis providers (ADR 0045), each driven only
//! through its official client:
//!
//! * `claude_code`: a Claude Pro/Max subscription through the `claude` CLI,
//!   run headless (`claude -p`). The CLI sees a fresh temporary directory
//!   that holds only the file (under an internal name), may use only its
//!   read-only `Read` tool there (`--restricted --tools Read`), loads no
//!   settings, hooks or MCP servers, and keeps no session. API-key
//!   variables are removed from its environment, so the CLI's own sign-in
//!   is the only credential and a subscription is never billed as API use.
//! * `codex`: a ChatGPT plan through the official Codex runtime, in a fresh
//!   app-server thread with approvals `never` and a read-only sandbox
//!   limited to a private directory, without network. It takes text and
//!   images (`localImage`); a PDF reaches it as rendered page images,
//!   because the runtime has no PDF input.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use docmonk_config::model::AiConfig;
use docmonk_core::errors::DocMonkError;
use serde_json::Value;

use crate::analysis::{
    instruction, parse_result, validate_value, AiAnalyzer, AnalysisError, AnalysisErrorKind,
    AnalysisInput, AnalysisResult, InputKind, InputPart, SYSTEM_PROMPT,
};
use crate::analysis_http::structured_output_schema;
use crate::runtime::which;

pub const CLAUDE_CODE_PROVIDER_ID: &str = "claude_code";
/// A fixed executable name, never a path from (untrusted) config.
pub const CLAUDE_EXECUTABLE: &str = "claude";
/// Credentials that would replace the CLI's own sign-in (an API key would
/// bill the request as API usage instead of the subscription).
pub const CLAUDE_CONFLICTING_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "OPENAI_API_KEY",
    "DOCMONK_AI_API_KEY",
    "DOCMONK_OPENAI_COMPATIBLE_API_KEY",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "COPILOT_GITHUB_TOKEN",
];
pub const CLAUDE_SIGN_IN_HINT: &str =
    "sign in with the Claude CLI itself: run `claude` and use /login (a Claude Pro or Max plan)";

/// One finished `claude -p` run.
#[derive(Debug)]
pub struct CliOutput {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// How the CLI is found and started. The default is the process `PATH`
/// and environment; tests point it at a fake CLI.
#[derive(Debug, Clone, Default)]
pub struct CliOptions {
    /// Search path for the fixed executable name (default: `PATH`).
    pub search_path: Option<std::ffi::OsString>,
    /// Extra variables set on the child before the conflicting ones are
    /// removed (so a credential here is still stripped).
    pub env: Vec<(String, String)>,
}

/// Runs `claude` with `args` in `cwd`, without the conflicting variables,
/// killing it at the deadline.
pub fn run_claude(
    args: &[String],
    cwd: Option<&std::path::Path>,
    timeout_s: f64,
    opts: &CliOptions,
) -> Result<CliOutput, AnalysisError> {
    let found = match &opts.search_path {
        Some(p) => crate::runtime::which_in(CLAUDE_EXECUTABLE, p),
        None => which(CLAUDE_EXECUTABLE),
    };
    if found.is_none() {
        return Err(AnalysisError::new(
            AnalysisErrorKind::NotSignedIn,
            "the Claude CLI ('claude') is not installed or not on PATH; install Claude Code and \
             sign in with it to use the claude_code provider (see docs/providers).",
        ));
    }
    let mut cmd = Command::new(CLAUDE_EXECUTABLE);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    if let Some(p) = &opts.search_path {
        // On Unix the child's PATH is also the program lookup path.
        cmd.env("PATH", p);
    }
    for (k, v) in &opts.env {
        cmd.env(k, v);
    }
    for name in CLAUDE_CONFLICTING_ENV {
        cmd.env_remove(name);
    }
    let mut child = cmd.spawn().map_err(|e| {
        AnalysisError::new(
            AnalysisErrorKind::Failed,
            format!("could not start the '{CLAUDE_EXECUTABLE}' CLI: {e}"),
        )
    })?;
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
            return Err(AnalysisError::new(
                AnalysisErrorKind::Timeout,
                format!("the Claude CLI did not finish within {timeout_s:.0}s; it was stopped"),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Ok(CliOutput {
        code: status.code(),
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

/// Classes a failed CLI run by its own words.
pub fn classify_claude_failure(text: &str) -> AnalysisErrorKind {
    let lower = text.to_lowercase();
    let has = |s: &str| lower.contains(s);
    if has("usage limit") || has("rate limit") || has("limit reached") || has("429") {
        AnalysisErrorKind::Quota
    } else if has("/login")
        || has("not logged in")
        || has("log in")
        || has("sign in")
        || has("invalid api key")
        || has("authentication")
        || has("401")
    {
        AnalysisErrorKind::NotSignedIn
    } else {
        AnalysisErrorKind::Failed
    }
}

/// The internal file name a part is written under (the sender's name never
/// touches the file system).
fn part_file_name(i: usize, part: &InputPart) -> String {
    match part {
        InputPart::Pdf { .. } => format!("part-{i}.pdf"),
        InputPart::Image { media_type, .. } => {
            let ext = match media_type.as_str() {
                "image/jpeg" => "jpg",
                "image/tiff" => "tiff",
                "image/gif" => "gif",
                "image/webp" => "webp",
                _ => "png",
            };
            format!("part-{i}.{ext}")
        }
        InputPart::Text { .. } => format!("part-{i}.txt"),
    }
}

/// Maps the CLI's `--output-format json` result to an analysis.
pub fn claude_result(
    out: &CliOutput,
    page_count: Option<i64>,
) -> Result<AnalysisResult, AnalysisError> {
    let v: Option<Value> = serde_json::from_str(out.stdout.trim()).ok();
    let failed = out.code != Some(0) || v.as_ref().is_none_or(|v| v["is_error"] == true);
    if failed {
        let detail = v
            .as_ref()
            .and_then(|v| v["result"].as_str())
            .map(str::to_owned)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                if out.stderr.trim().is_empty() {
                    out.stdout.trim().to_owned()
                } else {
                    out.stderr.trim().to_owned()
                }
            });
        let kind = classify_claude_failure(&detail);
        let hint = if kind == AnalysisErrorKind::NotSignedIn {
            format!(". Then {CLAUDE_SIGN_IN_HINT}")
        } else {
            String::new()
        };
        let head: String = detail.chars().take(500).collect();
        return Err(AnalysisError::new(
            kind,
            format!("the Claude CLI failed: {head}{hint}"),
        ));
    }
    let v = v.expect("checked above");
    if v["stop_reason"] == "refusal" {
        return Err(AnalysisError::new(
            AnalysisErrorKind::Refused,
            "Claude declined to analyse the file",
        ));
    }
    match &v["structured_output"] {
        Value::Object(_) => validate_value(v["structured_output"].clone(), page_count),
        _ => parse_result(v["result"].as_str().unwrap_or_default(), page_count),
    }
}

/// Claude Pro/Max through the Claude CLI.
pub struct ClaudeCodeAnalyzer {
    model: String,
    timeout: f64,
    /// How the CLI is found and started (tests point it at a fake CLI).
    pub cli: CliOptions,
}

impl ClaudeCodeAnalyzer {
    pub fn new(model: String, timeout: f64) -> Self {
        Self {
            model,
            timeout,
            cli: CliOptions::default(),
        }
    }

    /// The CLI arguments for one analysis (public for tests).
    pub fn args(&self, prompt: &str, dir: &std::path::Path) -> Vec<String> {
        let mut args: Vec<String> = [
            "-p",
            prompt,
            "--output-format",
            "json",
            "--no-session-persistence",
            "--restricted",
            "--strict-mcp-config",
            "--tools",
            "Read",
            "--allowedTools",
            "Read",
            "--permission-mode",
            "dontAsk",
            "--append-system-prompt",
            SYSTEM_PROMPT,
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        args.push("--add-dir".into());
        args.push(dir.display().to_string());
        args.push("--json-schema".into());
        args.push(structured_output_schema().to_string());
        if !self.model.is_empty() {
            args.push("--model".into());
            args.push(self.model.clone());
        }
        args
    }
}

impl AiAnalyzer for ClaudeCodeAnalyzer {
    fn provider_id(&self) -> &'static str {
        CLAUDE_CODE_PROVIDER_ID
    }

    fn model(&self) -> &str {
        if self.model.is_empty() {
            "default"
        } else {
            &self.model
        }
    }

    fn accepts(&self, _kind: InputKind) -> bool {
        // The CLI's Read tool reads PDFs, images and text.
        true
    }

    fn analyze(&self, input: &AnalysisInput) -> Result<AnalysisResult, AnalysisError> {
        let dir = tempfile_dir()?;
        let mut names = Vec::new();
        for (i, part) in input.parts.iter().enumerate() {
            let name = part_file_name(i, part);
            let bytes: &[u8] = match part {
                InputPart::Pdf { bytes } | InputPart::Image { bytes, .. } => bytes,
                InputPart::Text { text, .. } => text.as_bytes(),
            };
            std::fs::write(dir.path().join(&name), bytes).map_err(|e| {
                AnalysisError::new(
                    AnalysisErrorKind::Failed,
                    format!("cannot stage the file: {e}"),
                )
            })?;
            let label = match part {
                InputPart::Text { name: Some(n), .. } => format!("{name} (container part {n})"),
                _ => name,
            };
            names.push(label);
        }
        let prompt = format!(
            "Read these files in the current directory, in order: {}. Together they are the file \
             `{}`.\n\n{}",
            names.join(", "),
            input.file_name,
            instruction(input)
        );
        let out = run_claude(
            &self.args(&prompt, dir.path()),
            Some(dir.path()),
            self.timeout,
            &self.cli,
        )?;
        claude_result(&out, input.page_count)
    }
}

/// A private temporary directory, removed on drop.
fn tempfile_dir() -> Result<TempDir, AnalysisError> {
    TempDir::new().map_err(|e| {
        AnalysisError::new(
            AnalysisErrorKind::Failed,
            format!("cannot create a temporary directory: {e}"),
        )
    })
}

/// A minimal owned temporary directory (no extra dependency).
pub struct TempDir(std::path::PathBuf);

impl TempDir {
    pub fn new() -> std::io::Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir();
        for _ in 0..16 {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            let p = base.join(format!(
                "docmonk-analysis-{}-{}-{nanos}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&p) {
                Ok(()) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ =
                            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700));
                    }
                    return Ok(Self(p));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::other("no free temporary directory name"))
    }

    pub fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A runtime error from the codex app-server as an analysis error.
pub fn analysis_error_from(e: &DocMonkError) -> AnalysisError {
    let kind = match e.class() {
        Some("AiQuotaExhaustedError") => AnalysisErrorKind::Quota,
        Some("AiAuthenticationRequiredError") | Some("AiRuntimeUnavailableError") => {
            AnalysisErrorKind::NotSignedIn
        }
        Some("AiTimeoutError") => AnalysisErrorKind::Timeout,
        Some("AiPolicyBlockedError") => AnalysisErrorKind::Refused,
        Some("AiInvalidResponseError") => AnalysisErrorKind::InvalidOutput,
        _ => AnalysisErrorKind::Failed,
    };
    AnalysisError::new(kind, e.message().to_owned())
}

/// A ChatGPT plan through the Codex runtime.
pub struct CodexAnalyzer {
    model: String,
    runtime: Mutex<crate::codex::CodexRuntime>,
}

impl CodexAnalyzer {
    pub fn new(ai: &AiConfig) -> Self {
        Self {
            model: ai.model.clone(),
            runtime: Mutex::new(crate::codex::CodexRuntime::new(ai)),
        }
    }
}

impl AiAnalyzer for CodexAnalyzer {
    fn provider_id(&self) -> &'static str {
        "codex"
    }

    fn model(&self) -> &str {
        if self.model.is_empty() {
            "default"
        } else {
            &self.model
        }
    }

    fn accepts(&self, kind: InputKind) -> bool {
        // PDFs reach codex as rendered page images (the document processor
        // adapts them; the runtime has no PDF input).
        matches!(kind, InputKind::Text | InputKind::Image)
    }

    fn analyze(&self, input: &AnalysisInput) -> Result<AnalysisResult, AnalysisError> {
        if let Some(p) = input.parts.iter().find(|p| !self.accepts(p.kind())) {
            return Err(AnalysisError::new(
                AnalysisErrorKind::UnsupportedFormat,
                format!(
                    "the codex provider takes text and images; {} ({} input) must be sent as \
                     page images",
                    input.file_name,
                    p.kind().as_str()
                ),
            ));
        }
        // Images go to the runtime as files in a private directory, which
        // is also the turn's only readable root.
        let dir = tempfile_dir()?;
        let mut user = String::new();
        let mut images = Vec::new();
        for (i, p) in input.parts.iter().enumerate() {
            match p {
                InputPart::Text {
                    name: Some(n),
                    text,
                } => user.push_str(&format!("<part name=\"{n}\">\n{text}\n</part>\n")),
                InputPart::Text { name: None, text } => {
                    user.push_str(text);
                    user.push('\n');
                }
                InputPart::Image { .. } => {
                    let path = dir.path().join(part_file_name(i, p));
                    let InputPart::Image { bytes, .. } = p else {
                        unreachable!()
                    };
                    std::fs::write(&path, bytes).map_err(|e| {
                        AnalysisError::new(
                            AnalysisErrorKind::Failed,
                            format!("cannot stage an image: {e}"),
                        )
                    })?;
                    images.push(path);
                }
                InputPart::Pdf { .. } => unreachable!("refused above"),
            }
        }
        user.push('\n');
        user.push_str(&instruction(input));
        let mut rt = self.runtime.lock().unwrap_or_else(|p| p.into_inner());
        let out = rt
            .run_turn(
                SYSTEM_PROMPT,
                &user,
                &images,
                dir.path(),
                &self.model,
                Some(structured_output_schema()),
            )
            .map_err(|e| analysis_error_from(&e))?;
        parse_result(&out.text, input.page_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(code: i32, stdout: &str) -> CliOutput {
        CliOutput {
            code: Some(code),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    #[test]
    fn claude_failures_are_classed() {
        assert_eq!(
            classify_claude_failure("Claude AI usage limit reached|1767323045"),
            AnalysisErrorKind::Quota
        );
        assert_eq!(
            classify_claude_failure("Invalid API key · Please run /login"),
            AnalysisErrorKind::NotSignedIn
        );
        assert_eq!(classify_claude_failure("boom"), AnalysisErrorKind::Failed);
    }

    #[test]
    fn claude_results_map_to_analysis_or_errors() {
        let good = serde_json::json!({
            "schema_version": 1, "title": null, "author": null, "language": null,
            "summary": null, "keywords": [], "page_count": null,
            "chunks": [{"kind": "text", "heading_path": [], "heading_level": null,
                        "page_start": null, "page_end": null, "text": "Hello",
                        "embedding_text": null, "table_rows": null}],
            "code_references": []
        });
        let structured = serde_json::json!({"type": "result", "is_error": false,
            "result": "", "structured_output": good, "stop_reason": "end_turn"});
        assert_eq!(
            claude_result(&out(0, &structured.to_string()), None)
                .unwrap()
                .chunks[0]
                .text,
            "Hello"
        );
        let text = serde_json::json!({"is_error": false, "result": good.to_string()});
        assert!(claude_result(&out(0, &text.to_string()), None).is_ok());
        let limit =
            serde_json::json!({"is_error": true, "result": "Claude AI usage limit reached"});
        assert_eq!(
            claude_result(&out(1, &limit.to_string()), None)
                .unwrap_err()
                .kind,
            AnalysisErrorKind::Quota
        );
        let refused =
            serde_json::json!({"is_error": false, "result": "", "stop_reason": "refusal"});
        assert_eq!(
            claude_result(&out(0, &refused.to_string()), None)
                .unwrap_err()
                .kind,
            AnalysisErrorKind::Refused
        );
        let garbage = out(0, "not json");
        assert_eq!(
            claude_result(&garbage, None).unwrap_err().kind,
            AnalysisErrorKind::Failed
        );
    }

    #[test]
    fn claude_args_lock_the_cli_down() {
        let a = ClaudeCodeAnalyzer::new("opus".into(), 60.0);
        let args = a.args("PROMPT", std::path::Path::new("/tmp/x"));
        let joined = args.join(" ");
        for want in [
            "-p PROMPT",
            "--output-format json",
            "--no-session-persistence",
            "--restricted",
            "--strict-mcp-config",
            "--tools Read",
            "--permission-mode dontAsk",
            "--add-dir /tmp/x",
            "--model opus",
            "--json-schema {",
        ] {
            assert!(joined.contains(want), "{want} missing in {joined}");
        }
        assert!(!joined.contains("dangerously"));
        assert!(!joined.contains("bypassPermissions"));
    }

    #[test]
    fn temp_dirs_are_private_and_removed() {
        let p = {
            let d = TempDir::new().unwrap();
            assert!(d.path().is_dir());
            d.path().to_path_buf()
        };
        assert!(!p.exists());
    }
}
