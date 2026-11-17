//! The API-key analysis providers (ADR 0045): the Anthropic Messages API
//! (ANTHROPIC_API_KEY) and the OpenAI Responses API (OPENAI_API_KEY).
//!
//! Both send the file natively where the API accepts it (PDF and images),
//! ask for structured JSON output against the analysis schema, and make
//! exactly one request: a failure is classed for the caller and never
//! retried here.

use std::time::Duration;

use reqwest::blocking::Client;
use serde_json::{json, Value};

use crate::analysis::{
    analysis_json_schema, base64, instruction, parse_result, AiAnalyzer, AnalysisError,
    AnalysisErrorKind, AnalysisInput, AnalysisResult, InputKind, InputPart, SYSTEM_PROMPT,
};

/// The default Anthropic analysis model.
pub const ANTHROPIC_ANALYSIS_MODEL: &str = "claude-opus-5-5";
/// The Messages API version analysis requests are sent with.
pub const ANTHROPIC_ANALYSIS_API_VERSION: &str = "2026-10-21";
/// Server-side refusal fallback (`fallbacks: "default"`).
pub const ANTHROPIC_FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// The default OpenAI analysis model (PDF and image input, structured output).
pub const OPENAI_ANALYSIS_MODEL: &str = "gpt-4o-mini";
/// Anthropic's limit on one request body.
pub const ANTHROPIC_MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;

fn client(timeout: f64) -> Client {
    let mut b = Client::builder();
    if timeout.is_finite() && timeout > 0.0 {
        b = b.timeout(Duration::from_secs_f64(timeout));
    }
    b.build().unwrap_or_else(|_| Client::new())
}

/// The analysis schema in the subset structured output accepts: nullable
/// fields as `anyOf`, and no array length constraints (they are validated
/// locally by [`parse_result`]).
pub fn structured_output_schema() -> Value {
    fn walk(v: &mut Value) {
        match v {
            Value::Object(m) => {
                m.remove("minItems");
                if let Some(Value::Array(types)) = m.get("type").cloned() {
                    let non_null: Vec<Value> =
                        types.iter().filter(|t| *t != "null").cloned().collect();
                    if non_null.len() == 1 && types.len() == 2 {
                        m.remove("type");
                        let mut base = serde_json::Map::new();
                        base.insert("type".into(), non_null[0].clone());
                        if let Some(items) = m.remove("items") {
                            base.insert("items".into(), items);
                        }
                        let mut base = Value::Object(base);
                        walk(&mut base);
                        m.insert("anyOf".into(), json!([base, {"type": "null"}]));
                    }
                }
                for (_, child) in m.iter_mut() {
                    walk(child);
                }
            }
            Value::Array(a) => a.iter_mut().for_each(walk),
            _ => {}
        }
    }
    let mut s = analysis_json_schema();
    walk(&mut s);
    s
}

/// Classes an HTTP failure.
fn status_error(provider: &str, status: u16, body: &str) -> AnalysisError {
    let kind = match status {
        401 | 403 => AnalysisErrorKind::NotSignedIn,
        429 => AnalysisErrorKind::Quota,
        408 => AnalysisErrorKind::Timeout,
        413 => AnalysisErrorKind::TooLarge,
        400 | 404 | 422 => AnalysisErrorKind::Rejected,
        _ => AnalysisErrorKind::Failed,
    };
    let head: String = body.chars().take(500).collect();
    AnalysisError::new(kind, format!("{provider} returned HTTP {status}: {head}"))
}

fn transport_error(provider: &str, e: &reqwest::Error) -> AnalysisError {
    if e.is_timeout() {
        AnalysisError::new(
            AnalysisErrorKind::Timeout,
            format!("{provider} request timed out"),
        )
    } else {
        AnalysisError::new(
            AnalysisErrorKind::Failed,
            format!("{provider} connection error"),
        )
    }
}

fn send(
    provider: &str,
    request: reqwest::blocking::RequestBuilder,
) -> Result<Value, AnalysisError> {
    let response = request.send().map_err(|e| transport_error(provider, &e))?;
    let status = response.status().as_u16();
    let body = response.text().map_err(|e| transport_error(provider, &e))?;
    if status >= 400 {
        return Err(status_error(provider, status, &body));
    }
    serde_json::from_str(&body).map_err(|e| {
        AnalysisError::new(
            AnalysisErrorKind::Failed,
            format!("{provider} returned a non-JSON response: {e}"),
        )
    })
}

fn too_large(provider: &str, input: &AnalysisInput, limit: usize) -> Option<AnalysisError> {
    // Base64 grows binary parts by a third.
    let encoded = input.total_bytes() / 3 * 4 + 4096;
    (encoded > limit).then(|| {
        AnalysisError::new(
            AnalysisErrorKind::TooLarge,
            format!(
                "{} ({} bytes) exceeds the {provider} request limit",
                input.file_name,
                input.total_bytes()
            ),
        )
    })
}

fn text_block(name: &Option<String>, text: &str) -> String {
    match name {
        Some(n) => format!("<part name=\"{n}\">\n{text}\n</part>"),
        None => text.to_owned(),
    }
}

/// Anthropic Messages API analysis.
pub struct AnthropicAnalyzer {
    api_key: String,
    model: String,
    max_tokens: i64,
    url: String,
    client: Client,
}

impl AnthropicAnalyzer {
    pub fn new(
        api_key: String,
        model: String,
        base_url: &str,
        timeout: f64,
        max_tokens: i64,
    ) -> Self {
        Self {
            api_key,
            model,
            max_tokens,
            url: format!("{}/v1/messages", base_url.trim_end_matches('/')),
            client: client(timeout),
        }
    }

    /// The request body (public for tests).
    pub fn body(&self, input: &AnalysisInput) -> Value {
        let mut content: Vec<Value> = input
            .parts
            .iter()
            .map(|p| match p {
                InputPart::Pdf { bytes } => json!({
                    "type": "document",
                    "source": {"type": "base64", "media_type": "application/pdf", "data": base64(bytes)},
                }),
                InputPart::Image { media_type, bytes } => json!({
                    "type": "image",
                    "source": {"type": "base64", "media_type": media_type, "data": base64(bytes)},
                }),
                InputPart::Text { name, text } => json!({"type": "text", "text": text_block(name, text)}),
            })
            .collect();
        content.push(json!({"type": "text", "text": instruction(input)}));
        json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "system": SYSTEM_PROMPT,
            "messages": [{"role": "user", "content": content}],
            "output_config": {
                "effort": "medium",
                "format": {"type": "json_schema", "schema": structured_output_schema()},
            },
            "fallbacks": "default",
        })
    }
}

impl AiAnalyzer for AnthropicAnalyzer {
    fn provider_id(&self) -> &'static str {
        "anthropic"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn accepts(&self, _kind: InputKind) -> bool {
        true
    }

    fn analyze(&self, input: &AnalysisInput) -> Result<AnalysisResult, AnalysisError> {
        if let Some(e) = too_large("anthropic", input, ANTHROPIC_MAX_REQUEST_BYTES) {
            return Err(e);
        }
        let r = send(
            "anthropic",
            self.client
                .post(&self.url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", ANTHROPIC_ANALYSIS_API_VERSION)
                .header("anthropic-beta", ANTHROPIC_FALLBACK_BETA)
                .header("accept", "application/json")
                .json(&self.body(input)),
        )?;
        match r["stop_reason"].as_str() {
            Some("refusal") => {
                return Err(AnalysisError::new(
                    AnalysisErrorKind::Refused,
                    format!(
                        "anthropic declined to analyse {} ({})",
                        input.file_name,
                        r["stop_details"]["category"]
                            .as_str()
                            .unwrap_or("no category")
                    ),
                ))
            }
            Some("max_tokens") => {
                return Err(AnalysisError::invalid(format!(
                    "the analysis of {} was cut off at max_tokens ({}); raise \
                     documents.analysis.max_output_tokens",
                    input.file_name, self.max_tokens
                )))
            }
            _ => {}
        }
        let text: String = r["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect();
        parse_result(&text, input.page_count)
    }
}

/// OpenAI Responses API analysis.
pub struct OpenAiAnalyzer {
    api_key: String,
    model: String,
    max_tokens: i64,
    url: String,
    client: Client,
}

impl OpenAiAnalyzer {
    pub fn new(
        api_key: String,
        model: String,
        base_url: &str,
        timeout: f64,
        max_tokens: i64,
    ) -> Self {
        Self {
            api_key,
            model,
            max_tokens,
            url: format!("{}/responses", base_url.trim_end_matches('/')),
            client: client(timeout),
        }
    }

    /// The request body (public for tests).
    pub fn body(&self, input: &AnalysisInput) -> Value {
        let mut content: Vec<Value> = input
            .parts
            .iter()
            .map(|p| match p {
                InputPart::Pdf { bytes } => json!({
                    "type": "input_file",
                    "filename": input.file_name,
                    "file_data": format!("data:application/pdf;base64,{}", base64(bytes)),
                }),
                InputPart::Image { media_type, bytes } => json!({
                    "type": "input_image",
                    "image_url": format!("data:{media_type};base64,{}", base64(bytes)),
                }),
                InputPart::Text { name, text } => {
                    json!({"type": "input_text", "text": text_block(name, text)})
                }
            })
            .collect();
        content.push(json!({"type": "input_text", "text": instruction(input)}));
        json!({
            "model": self.model,
            "instructions": SYSTEM_PROMPT,
            "input": [{"role": "user", "content": content}],
            "max_output_tokens": self.max_tokens,
            "text": {"format": {
                "type": "json_schema",
                "name": "docmonk_analysis",
                "strict": true,
                "schema": structured_output_schema(),
            }},
        })
    }
}

impl AiAnalyzer for OpenAiAnalyzer {
    fn provider_id(&self) -> &'static str {
        "openai"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn accepts(&self, _kind: InputKind) -> bool {
        true
    }

    fn analyze(&self, input: &AnalysisInput) -> Result<AnalysisResult, AnalysisError> {
        if let Some(e) = too_large("openai", input, ANTHROPIC_MAX_REQUEST_BYTES) {
            return Err(e);
        }
        let r = send(
            "openai",
            self.client
                .post(&self.url)
                .header("authorization", format!("Bearer {}", self.api_key))
                .header("accept", "application/json")
                .json(&self.body(input)),
        )?;
        if r["status"] == "incomplete" {
            return Err(AnalysisError::invalid(format!(
                "the analysis of {} is incomplete ({}); raise documents.analysis.max_output_tokens",
                input.file_name,
                r["incomplete_details"]["reason"]
                    .as_str()
                    .unwrap_or("unknown reason")
            )));
        }
        let mut text = String::new();
        for item in r["output"].as_array().into_iter().flatten() {
            for c in item["content"].as_array().into_iter().flatten() {
                match c["type"].as_str() {
                    Some("output_text") => text.push_str(c["text"].as_str().unwrap_or_default()),
                    Some("refusal") => {
                        return Err(AnalysisError::new(
                            AnalysisErrorKind::Refused,
                            format!(
                                "openai declined to analyse {}: {}",
                                input.file_name,
                                c["refusal"].as_str().unwrap_or_default()
                            ),
                        ))
                    }
                    _ => {}
                }
            }
        }
        parse_result(&text, input.page_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_schema_has_no_type_unions_or_length_constraints() {
        let s = structured_output_schema().to_string();
        assert!(!s.contains("minItems"));
        assert!(!s.contains(r#""type":["#), "{s}");
        assert!(s.contains("anyOf"));
        assert!(s.contains(r#""additionalProperties":false"#));
    }

    #[test]
    fn statuses_are_classed() {
        let k = |s| status_error("x", s, "").kind;
        assert_eq!(k(401), AnalysisErrorKind::NotSignedIn);
        assert_eq!(k(429), AnalysisErrorKind::Quota);
        assert_eq!(k(529), AnalysisErrorKind::Failed);
        assert_eq!(k(413), AnalysisErrorKind::TooLarge);
        assert_eq!(k(400), AnalysisErrorKind::Rejected);
    }
}
