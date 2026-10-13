//! AI providers for `ragmonk ask`. A provider only consumes the evidence `explore` already
//! assembled; nothing here reads a database or ranks anything.
//!
//! * [`prompt`]: the shared evidence prompt and request/answer shapes.
//! * [`http`]: OpenAI, OpenAI-compatible, Anthropic and Ollama over HTTP.
//! * [`factory`]: provider selection with the `privacy.external_ai_allowed`
//!   gate applied before anything is constructed.
//! * [`registry`]: the provider capability table.
//! * [`transport`], [`codex`], [`copilot`]: the subscription runtimes.

pub mod codex;
pub mod copilot;
pub mod errors;
pub mod factory;
pub mod http;
pub mod prompt;
pub mod registry;
pub mod runtime;
pub mod text;
pub mod transport;

pub use factory::create_provider;
pub use prompt::{AiAnswer, AiRequest, AiUsage};

use docmonk_core::errors::RagMonkError;

/// Turns an [`AiRequest`] into an [`AiAnswer`].
pub trait AiProvider: Send {
    fn answer(&mut self, request: &AiRequest) -> Result<AiAnswer, RagMonkError>;
}
