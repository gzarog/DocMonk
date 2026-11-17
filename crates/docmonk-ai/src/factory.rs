//! Builds the configured provider. The
//! `privacy.external_ai_allowed` gate is applied before any provider or
//! client is constructed. Ollama is exempt only when its configured host
//! is loopback; a remote Ollama is gated like any cloud provider.

use docmonk_config::model::{AiConfig, AnalysisConfig, PrivacyConfig};
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
        "claude_code" => {
            require_external_ai_allowed(privacy, "claude_code")?;
            Ok(Box::new(crate::claude_code::ClaudeCodeProvider::new(ai)))
        }
        _ => Err(not_configured(format!(
            "unknown ai.provider={}; expected one of openai, anthropic, ollama, \
             openai_compatible, codex, github_copilot, claude_code",
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

/// Builds the file-analysis provider for `documents.analysis` (ADR 0045).
/// The privacy gate runs first: every analysis provider sends file content
/// off this machine. `env` reads environment variables (a test seam).
pub fn create_analyzer_with_env(
    cfg: &AnalysisConfig,
    privacy: &PrivacyConfig,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<std::sync::Arc<dyn crate::analysis::AiAnalyzer>, DocMonkError> {
    use crate::analysis_http::{
        AnthropicAnalyzer, OpenAiAnalyzer, ANTHROPIC_ANALYSIS_MODEL, OPENAI_ANALYSIS_MODEL,
    };
    use crate::analysis_subscription::{ClaudeCodeAnalyzer, CodexAnalyzer};
    let provider = cfg.provider.trim().to_lowercase();
    if provider.is_empty() {
        return Err(not_configured(
            "documents.analysis.mode is 'ai' but no documents.analysis.provider is set. Set one \
             with: docmonk config set documents.analysis.provider \
             <anthropic|openai|claude_code|codex>",
        ));
    }
    if !docmonk_config::model::ANALYSIS_PROVIDERS.contains(&provider.as_str()) {
        return Err(not_configured(format!(
            "unknown documents.analysis.provider={provider}; expected one of {}",
            docmonk_config::model::ANALYSIS_PROVIDERS.join(", ")
        )));
    }
    if !privacy.external_ai_allowed {
        return Err(privacy_blocked(format!(
            "documents.analysis.provider='{provider}' sends every document file to a network \
             endpoint outside this machine, which requires privacy.external_ai_allowed=true (it \
             defaults to false). Set it with: docmonk config set privacy.external_ai_allowed true"
        )));
    }
    let key = |name: &str| env(name).filter(|v| !v.is_empty());
    let max_tokens = cfg.max_output_tokens;
    Ok(match provider.as_str() {
        "anthropic" => {
            let api_key = key("ANTHROPIC_API_KEY").ok_or_else(|| {
                not_configured(
                    "documents.analysis.provider=anthropic requires the ANTHROPIC_API_KEY env var",
                )
            })?;
            let base = key("ANTHROPIC_BASE_URL")
                .unwrap_or_else(|| crate::http::ANTHROPIC_DEFAULT_BASE_URL.into());
            std::sync::Arc::new(AnthropicAnalyzer::new(
                api_key,
                or_default(&cfg.model, ANTHROPIC_ANALYSIS_MODEL),
                &base,
                cfg.timeout_seconds,
                max_tokens,
            ))
        }
        "openai" => {
            let api_key = key("OPENAI_API_KEY").ok_or_else(|| {
                not_configured(
                    "documents.analysis.provider=openai requires the OPENAI_API_KEY env var",
                )
            })?;
            let base = key("OPENAI_BASE_URL")
                .unwrap_or_else(|| crate::http::OPENAI_DEFAULT_BASE_URL.into());
            std::sync::Arc::new(OpenAiAnalyzer::new(
                api_key,
                or_default(&cfg.model, OPENAI_ANALYSIS_MODEL),
                &base,
                cfg.timeout_seconds,
                max_tokens,
            ))
        }
        "claude_code" => std::sync::Arc::new(ClaudeCodeAnalyzer::new(
            cfg.model.clone(),
            cfg.timeout_seconds,
        )),
        "codex" => {
            let ai = AiConfig {
                provider: "codex".into(),
                model: cfg.model.clone(),
                base_url: None,
                timeout_seconds: cfg.timeout_seconds,
            };
            std::sync::Arc::new(CodexAnalyzer::new(&ai))
        }
        _ => unreachable!("checked against ANALYSIS_PROVIDERS"),
    })
}

/// [`create_analyzer_with_env`] over the process environment.
pub fn create_analyzer(
    cfg: &AnalysisConfig,
    privacy: &PrivacyConfig,
) -> Result<std::sync::Arc<dyn crate::analysis::AiAnalyzer>, DocMonkError> {
    create_analyzer_with_env(cfg, privacy, &|k| std::env::var(k).ok())
}

#[cfg(test)]
mod analysis_tests {
    use super::*;

    fn cfg(provider: &str) -> AnalysisConfig {
        AnalysisConfig {
            mode: "ai".into(),
            provider: provider.into(),
            ..AnalysisConfig::default()
        }
    }

    #[test]
    fn analyzers_need_privacy_consent_then_their_credentials() {
        let closed = PrivacyConfig::default();
        let open = PrivacyConfig {
            external_ai_allowed: true,
        };
        let none = |_: &str| None;
        for p in ["anthropic", "openai", "claude_code", "codex"] {
            let e = create_analyzer_with_env(&cfg(p), &closed, &none)
                .err()
                .unwrap();
            assert_eq!(e.class(), Some("AiPrivacyBlockedError"), "{p}");
            assert_eq!(e.exit_code(), 8);
        }
        let e = create_analyzer_with_env(&cfg("anthropic"), &open, &none)
            .err()
            .unwrap();
        assert!(e.message().contains("ANTHROPIC_API_KEY"));
        let e = create_analyzer_with_env(&cfg("openai"), &open, &none)
            .err()
            .unwrap();
        assert!(e.message().contains("OPENAI_API_KEY"));
        let e = create_analyzer_with_env(&cfg(""), &open, &none)
            .err()
            .unwrap();
        assert_eq!(e.class(), Some("AiNotConfiguredError"));
        let key = |k: &str| (k == "ANTHROPIC_API_KEY").then(|| "sk-test".to_owned());
        let a = create_analyzer_with_env(&cfg("anthropic"), &open, &key).unwrap();
        assert_eq!(a.extracted_by(), "ai:anthropic:claude-opus-5-5");
        let c = create_analyzer_with_env(&cfg("claude_code"), &open, &none).unwrap();
        assert_eq!(c.provider_id(), "claude_code");
        let x = create_analyzer_with_env(&cfg("codex"), &open, &none).unwrap();
        assert!(!x.accepts(crate::analysis::InputKind::Pdf));
    }
}
