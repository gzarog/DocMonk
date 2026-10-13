//! The HTTP providers: OpenAI Chat Completions (and OpenAI-compatible
//! endpoints), the Anthropic Messages API and Ollama's `/api/chat`.
//!
//! Requests, response mapping and failure messages follow the
//! providers' official SDKs, including their messages: `Connection
//! error.`, `Request timed out.`, and `Error code: <status> - <body>` for
//! a JSON error body. There are no retries: a failure surfaces at once.

use std::time::Duration;

use reqwest::blocking::{Client, Response};
use serde_json::{json, Value};

use crate::errors::provider_error;
use crate::prompt::{build_messages, AiAnswer, AiRequest, AiUsage};
use crate::text::json_body;
use crate::AiProvider;
use docmonk_core::errors::DocMonkError;

pub const OPENAI_DEFAULT_MODEL: &str = "gpt-4o-mini";
pub const OPENAI_DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub const ANTHROPIC_DEFAULT_MODEL: &str = "claude-3-5-haiku-latest";
pub const ANTHROPIC_DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const ANTHROPIC_DEFAULT_MAX_TOKENS: i64 = 1024;
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
pub const OLLAMA_DEFAULT_BASE_URL: &str = "http://localhost:11434";
pub const OLLAMA_DEFAULT_MODEL: &str = "llama3.2";

fn client(timeout: f64) -> Client {
    let mut b = Client::builder();
    if timeout.is_finite() && timeout > 0.0 {
        b = b.timeout(Duration::from_secs_f64(timeout));
    }
    b.build().unwrap_or_else(|_| Client::new())
}

/// The SDK-style failure text for a request that got no response.
fn sdk_transport_error(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "Request timed out."
    } else {
        "Connection error."
    }
}

/// The SDK-style message for a non-2xx response.
fn sdk_status_error(status: u16, body: &str) -> String {
    match json_body(body) {
        Some(r) => format!("Error code: {status} - {r}"),
        None if !body.is_empty() => body.to_owned(),
        None => format!("Error code: {status}"),
    }
}

/// Sends an SDK-backed request; `label` prefixes failures like
/// `openai request failed: ...`. A 2xx body that is not JSON fails the
/// way `ask` wraps an unexpected error.
fn sdk_call(
    label: &str,
    request: reqwest::blocking::RequestBuilder,
) -> Result<Value, DocMonkError> {
    let response: Response = request.send().map_err(|e| {
        provider_error(format!(
            "{label} request failed: {}",
            sdk_transport_error(&e)
        ))
    })?;
    let status = response.status().as_u16();
    let body = response.text().map_err(|e| {
        provider_error(format!(
            "{label} request failed: {}",
            sdk_transport_error(&e)
        ))
    })?;
    if status >= 400 {
        return Err(provider_error(format!(
            "{label} request failed: {}",
            sdk_status_error(status, &body)
        )));
    }
    serde_json::from_str(&body)
        .map_err(|e| provider_error(format!("ai provider call failed: {}", e)))
}

fn int(v: Option<&Value>) -> Option<i64> {
    v.and_then(Value::as_i64)
}

fn model_or(v: &Value, fallback: &str) -> String {
    v.get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
        .unwrap_or(fallback)
        .to_owned()
}

/// OpenAI, or an OpenAI-compatible endpoint (`provider` names which).
pub struct OpenAiProvider {
    provider: &'static str,
    label: &'static str,
    api_key: String,
    model: String,
    url: String,
    client: Client,
}

impl OpenAiProvider {
    pub fn openai(api_key: String, model: String, base_url: Option<&str>, timeout: f64) -> Self {
        let env_base = std::env::var("OPENAI_BASE_URL")
            .ok()
            .filter(|s| !s.is_empty());
        let base = base_url
            .map(str::to_owned)
            .or(env_base)
            .unwrap_or_else(|| OPENAI_DEFAULT_BASE_URL.into());
        Self::new("openai", "openai", api_key, model, &base, timeout)
    }

    pub fn compatible(api_key: String, model: String, base_url: &str, timeout: f64) -> Self {
        // Some endpoints accept any credential; the official SDKs
        // require a non-empty one, so a placeholder is sent.
        let key = if api_key.is_empty() {
            "not-required".into()
        } else {
            api_key
        };
        Self::new(
            "openai_compatible",
            "openai-compatible",
            key,
            model,
            base_url,
            timeout,
        )
    }

    fn new(
        provider: &'static str,
        label: &'static str,
        api_key: String,
        model: String,
        base: &str,
        timeout: f64,
    ) -> Self {
        Self {
            provider,
            label,
            api_key,
            model,
            url: format!("{}/chat/completions", base.trim_end_matches('/')),
            client: client(timeout),
        }
    }
}

impl AiProvider for OpenAiProvider {
    fn answer(&mut self, request: &AiRequest) -> Result<AiAnswer, DocMonkError> {
        let body = json!({"model": self.model, "messages": build_messages(request)});
        let r = sdk_call(
            self.label,
            self.client
                .post(&self.url)
                .header("authorization", format!("Bearer {}", self.api_key))
                .header("accept", "application/json")
                .json(&body),
        )?;
        let text = r["choices"]
            .get(0)
            .and_then(|c| c["message"]["content"].as_str())
            .unwrap_or_default()
            .to_owned();
        let usage = &r["usage"];
        Ok(AiAnswer {
            text,
            provider: self.provider.into(),
            model: model_or(&r, &self.model),
            usage: AiUsage {
                input_tokens: int(usage.get("prompt_tokens")),
                output_tokens: int(usage.get("completion_tokens")),
            },
        })
    }
}

pub struct AnthropicProvider {
    api_key: String,
    model: String,
    max_tokens: i64,
    url: String,
    client: Client,
}

impl AnthropicProvider {
    pub fn new(api_key: String, model: String, base_url: Option<&str>, timeout: f64) -> Self {
        let env_base = std::env::var("ANTHROPIC_BASE_URL")
            .ok()
            .filter(|s| !s.is_empty());
        let base = base_url
            .map(str::to_owned)
            .or(env_base)
            .unwrap_or_else(|| ANTHROPIC_DEFAULT_BASE_URL.into());
        Self {
            api_key,
            model,
            max_tokens: ANTHROPIC_DEFAULT_MAX_TOKENS,
            url: format!("{}/v1/messages", base.trim_end_matches('/')),
            client: client(timeout),
        }
    }
}

impl AiProvider for AnthropicProvider {
    fn answer(&mut self, request: &AiRequest) -> Result<AiAnswer, DocMonkError> {
        let messages = build_messages(request);
        let all = messages.as_array().cloned().unwrap_or_default();
        let system = all
            .iter()
            .find(|m| m["role"] == "system")
            .map(|m| m["content"].clone());
        let users: Vec<Value> = all.into_iter().filter(|m| m["role"] != "system").collect();
        let mut body = json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "messages": users,
        });
        if let Some(system) = system {
            body["system"] = system;
        }
        let r = sdk_call(
            "anthropic",
            self.client
                .post(&self.url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("accept", "application/json")
                .json(&body),
        )?;
        let text: String = r["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect();
        let usage = &r["usage"];
        Ok(AiAnswer {
            text,
            provider: "anthropic".into(),
            model: model_or(&r, &self.model),
            usage: AiUsage {
                input_tokens: int(usage.get("input_tokens")),
                output_tokens: int(usage.get("output_tokens")),
            },
        })
    }
}

pub struct OllamaProvider {
    model: String,
    url: String,
    client: Client,
}

impl OllamaProvider {
    pub fn new(model: String, base_url: &str, timeout: f64) -> Self {
        Self {
            model,
            url: format!("{}/api/chat", base_url.trim_end_matches('/')),
            client: client(timeout),
        }
    }
}

impl AiProvider for OllamaProvider {
    fn answer(&mut self, request: &AiRequest) -> Result<AiAnswer, DocMonkError> {
        let body = json!({
            "model": self.model,
            "messages": build_messages(request),
            "stream": false,
        });
        let response = self
            .client
            .post(&self.url)
            .header("accept", "*/*")
            .json(&body)
            .send()
            .map_err(|e| provider_error(format!("ollama request failed: {e}")))?;
        let status = response.status().as_u16();
        let text = response
            .text()
            .map_err(|e| provider_error(format!("ollama request failed: {e}")))?;
        if status >= 400 {
            let head: String = text.chars().take(500).collect();
            return Err(provider_error(format!(
                "ollama returned HTTP {status}: {head}"
            )));
        }
        let data: Value = serde_json::from_str(&text)
            .map_err(|e| provider_error(format!("ollama returned a non-JSON response: {}", e)))?;
        Ok(AiAnswer {
            text: data["message"]["content"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            provider: "ollama".into(),
            model: model_or(&data, &self.model),
            usage: AiUsage {
                input_tokens: int(data.get("prompt_eval_count")),
                output_tokens: int(data.get("eval_count")),
            },
        })
    }
}
