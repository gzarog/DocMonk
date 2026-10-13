//! Builds the configured provider. The
//! `privacy.external_ai_allowed` gate is applied before any provider or
//! client is constructed. Ollama is exempt only when its configured host
//! is loopback; a remote Ollama is gated like any cloud provider.

use docmonk_config::model::{AiConfig, PrivacyConfig};
use docmonk_core::errors::DocMonkError;

use crate::errors::{not_configured, privacy_blocked};
use crate::http::{
    AnthropicProvider, OllamaProvider, OpenAiProvider, ANTHROPIC_DEFAULT_MODEL,
    OLLAMA_DEFAULT_BASE_URL, OLLAMA_DEFAULT_MODEL, OPENAI_DEFAULT_MODEL,
};
use crate::{codex, copilot, AiProvider};

/// The env var an OpenAI-compatible endpoint's key is read from.
pub const COMPATIBLE_API_KEY_ENV: &str = "DOCMONK_AI_API_KEY";

/// The host of `url` as a standard URL parser reports it:
/// lowercased, brackets stripped, `None` without a `scheme://`.
pub fn url_host(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit('@').next().unwrap_or_default();
    let host = if let Some(stripped) = host_port.strip_prefix('[') {
        stripped.split(']').next().unwrap_or_default()
    } else {
        host_port.split(':').next().unwrap_or_default()
    };
    (!host.is_empty()).then(|| host.to_lowercase())
}

pub fn is_local_host(base_url: &str) -> bool {
    url_host(base_url)
        .is_some_and(|h| matches!(h.as_str(), "localhost" | "127.0.0.1" | "::1" | "[::1]"))
}

pub fn require_external_ai_allowed(
    privacy: &PrivacyConfig,
    label: &str,
) -> Result<(), DocMonkError> {
    if privacy.external_ai_allowed {
        return Ok(());
    }
    Err(privacy_blocked(format!(
        "ai.provider='{label}' sends data to a network endpoint outside this machine, which \
         requires privacy.external_ai_allowed=true (it defaults to false). Set it with: docmonk \
         config set privacy.external_ai_allowed true"
    )))
}

fn or_default(model: &str, default: &str) -> String {
    if model.is_empty() {
        default.into()
    } else {
        model.into()
    }
}

/// Selects and builds the provider. `env` reads environment variables
/// (a seam for tests).
pub fn create_provider_with_env(
    ai: &AiConfig,
    privacy: &PrivacyConfig,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Box<dyn AiProvider>, DocMonkError> {
    let provider = ai.provider.trim().to_lowercase();
    let key = |name: &str| env(name).filter(|v| !v.is_empty());
    match provider.as_str() {
        "" | "none" => Err(not_configured(
            "no ai.provider is configured. Set one with: docmonk config set ai.provider \
             <openai|anthropic|ollama|openai_compatible>",
        )),
        "openai" => {
            require_external_ai_allowed(privacy, "openai")?;
            let api_key = key("OPENAI_API_KEY").ok_or_else(|| {
                not_configured("ai.provider=openai requires the OPENAI_API_KEY env var")
            })?;
            Ok(Box::new(OpenAiProvider::openai(
                api_key,
                or_default(&ai.model, OPENAI_DEFAULT_MODEL),
                ai.base_url.as_deref(),
                ai.timeout_seconds,
            )))
        }
        "anthropic" => {
            require_external_ai_allowed(privacy, "anthropic")?;
            let api_key = key("ANTHROPIC_API_KEY").ok_or_else(|| {
                not_configured("ai.provider=anthropic requires the ANTHROPIC_API_KEY env var")
            })?;
            Ok(Box::new(AnthropicProvider::new(
                api_key,
                or_default(&ai.model, ANTHROPIC_DEFAULT_MODEL),
                ai.base_url.as_deref(),
                ai.timeout_seconds,
            )))
        }
        "openai_compatible" => {
            require_external_ai_allowed(privacy, "openai_compatible")?;
            let base = ai
                .base_url
                .as_deref()
                .filter(|b| !b.is_empty())
                .ok_or_else(|| {
                    not_configured("ai.provider=openai_compatible requires ai.base_url")
                })?;
            if ai.model.is_empty() {
                return Err(not_configured(
                    "ai.provider=openai_compatible requires ai.model",
                ));
            }
            Ok(Box::new(OpenAiProvider::compatible(
                env(COMPATIBLE_API_KEY_ENV).unwrap_or_default(),
                ai.model.clone(),
                base,
                ai.timeout_seconds,
            )))
        }
        "ollama" => {
            let base = ai
                .base_url
                .clone()
                .filter(|b| !b.is_empty())
                .unwrap_or_else(|| OLLAMA_DEFAULT_BASE_URL.into());
            if !is_local_host(&base) {
                require_external_ai_allowed(privacy, "ollama (non-local base_url)")?;
            }
            Ok(Box::new(OllamaProvider::new(
                or_default(&ai.model, OLLAMA_DEFAULT_MODEL),
                &base,
                ai.timeout_seconds,
            )))
        }
        "codex" => {
            require_external_ai_allowed(privacy, "codex")?;
            Ok(Box::new(codex::CodexProvider::new(ai)))
        }
        "github_copilot" => {
            require_external_ai_allowed(privacy, "github_copilot")?;
            Ok(Box::new(copilot::CopilotProvider::new(ai)))
        }
        _ => Err(not_configured(format!(
            "unknown ai.provider={}; expected one of openai, anthropic, ollama, \
             openai_compatible, codex, github_copilot",
            crate::text::quoted(&serde_json::Value::String(ai.provider.clone()))
        ))),
    }
}

/// [`create_provider_with_env`] over the process environment.
pub fn create_provider(
    ai: &AiConfig,
    privacy: &PrivacyConfig,
) -> Result<Box<dyn AiProvider>, DocMonkError> {
    create_provider_with_env(ai, privacy, &|k| std::env::var(k).ok())
}
