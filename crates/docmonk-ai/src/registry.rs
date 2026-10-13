//! The provider capability table: data only, so
//! listing providers starts no runtime.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ProviderCapability {
    pub provider_id: &'static str,
    pub display_name: &'static str,
    pub auth_modes: &'static [&'static str],
    pub default_auth_mode: &'static str,
    pub cloud_egress: bool,
    pub subscription: bool,
    pub api_key_env: Option<&'static str>,
    pub model_discovery: &'static str,
    pub usage_reporting: &'static str,
    pub beta: bool,
    pub notes: &'static str,
}

const LOCAL_NOTE: &str = "Local inference; loopback host is exempt from the privacy flag.";

/// Every provider, name-sorted (the order `ragmonk ai providers` prints).
pub const REGISTRY: &[ProviderCapability] = &[
    ProviderCapability {
        provider_id: "anthropic",
        display_name: "Anthropic API",
        auth_modes: &["api_key"],
        default_auth_mode: "api_key",
        cloud_egress: true,
        subscription: false,
        api_key_env: Some("ANTHROPIC_API_KEY"),
        model_discovery: "static",
        usage_reporting: "tokens",
        beta: false,
        notes: "API-key provider; reads ANTHROPIC_API_KEY from the environment.",
    },
    ProviderCapability {
        provider_id: "codex",
        display_name: "ChatGPT via Codex (subscription)",
        auth_modes: &["chatgpt"],
        default_auth_mode: "chatgpt",
        cloud_egress: true,
        subscription: true,
        api_key_env: None,
        model_discovery: "dynamic",
        usage_reporting: "unknown",
        beta: true,
        notes: "Uses a signed-in ChatGPT account through the official Codex runtime; consumes \
                the account allowance. Beta until its version/isolation gates pass. Never reuses \
                OPENAI_API_KEY.",
    },
    ProviderCapability {
        provider_id: "github_copilot",
        display_name: "GitHub Copilot (subscription)",
        auth_modes: &["signed_in_user"],
        default_auth_mode: "signed_in_user",
        cloud_egress: true,
        subscription: true,
        api_key_env: None,
        model_discovery: "dynamic",
        usage_reporting: "unknown",
        beta: true,
        notes: "Uses the signed-in GitHub Copilot CLI credentials through the official SDK; \
                consumes the Copilot allowance. Never uses GH_TOKEN/GITHUB_TOKEN or a provider \
                key.",
    },
    ProviderCapability {
        provider_id: "ollama",
        display_name: "Ollama (local)",
        auth_modes: &["none"],
        default_auth_mode: "none",
        cloud_egress: false,
        subscription: false,
        api_key_env: None,
        model_discovery: "config",
        usage_reporting: "none",
        beta: false,
        notes: LOCAL_NOTE,
    },
    ProviderCapability {
        provider_id: "openai",
        display_name: "OpenAI API",
        auth_modes: &["api_key"],
        default_auth_mode: "api_key",
        cloud_egress: true,
        subscription: false,
        api_key_env: Some("OPENAI_API_KEY"),
        model_discovery: "static",
        usage_reporting: "tokens",
        beta: false,
        notes: "API-key provider; reads OPENAI_API_KEY from the environment.",
    },
    ProviderCapability {
        provider_id: "openai_compatible",
        display_name: "OpenAI-compatible endpoint",
        auth_modes: &["api_key"],
        default_auth_mode: "api_key",
        cloud_egress: true,
        subscription: false,
        api_key_env: Some("RAGMONK_OPENAI_COMPATIBLE_API_KEY"),
        model_discovery: "config",
        usage_reporting: "unknown",
        beta: false,
        notes: "Self-hosted or third-party endpoint; requires ai.base_url and ai.model.",
    },
];

/// Subscription providers, in registry-definition order.
pub const SUBSCRIPTION_PROVIDERS: &[&str] = &["codex", "github_copilot"];

pub fn get(provider_id: &str) -> Option<&'static ProviderCapability> {
    let id = provider_id.trim().to_lowercase();
    REGISTRY.iter().find(|c| c.provider_id == id)
}
