//! Subscription runtime lifecycle: the status snapshot,
//! the lifecycle trait `docmonk ai` drives, and executable lookup.

use std::path::PathBuf;

use docmonk_config::model::AiConfig;
use docmonk_core::errors::DocMonkError;
use serde_json::{json, Value};

use crate::errors::runtime_unavailable;
use crate::registry;

/// A connection snapshot; never carries a token or secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeStatus {
    pub provider_id: String,
    pub authenticated: bool,
    pub account: Option<String>,
    pub runtime_version: Option<String>,
    pub detail: String,
}

impl RuntimeStatus {
    pub fn to_json(&self) -> Value {
        json!({
            "provider": self.provider_id,
            "authenticated": self.authenticated,
            "account": self.account,
            "runtime_version": self.runtime_version,
            "detail": self.detail,
        })
    }
}

/// Sign-in, sign-out, state and models of an account-based provider.
pub trait SubscriptionRuntime {
    fn status(&mut self) -> Result<RuntimeStatus, DocMonkError>;
    fn login(&mut self) -> Result<RuntimeStatus, DocMonkError>;
    fn logout(&mut self) -> Result<(), DocMonkError>;
    fn models(&mut self) -> Result<Vec<String>, DocMonkError>;
}

/// The runtime for a subscription provider; nothing starts until used.
pub fn resolve_runtime(
    provider_id: &str,
    ai: &AiConfig,
) -> Result<Box<dyn SubscriptionRuntime>, DocMonkError> {
    let cap = registry::get(provider_id).filter(|c| c.subscription);
    match cap.map(|c| c.provider_id) {
        Some("codex") => Ok(Box::new(crate::codex::CodexRuntime::new(ai))),
        Some("github_copilot") => Ok(Box::new(crate::copilot::CopilotRuntime::new(ai))),
        _ => Err(runtime_unavailable(format!(
            "'{provider_id}' is not a subscription provider with a managed runtime"
        ))),
    }
}

/// `shutil.which`: `name` on `PATH` (with `PATHEXT` on Windows).
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(str::to_owned)
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path) {
        for ext in &exts {
            let candidate = dir.join(format!("{name}{ext}"));
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &std::path::Path) -> bool {
    p.is_file()
}
