//! LLM provider abstraction.
//!
//! The Provider trait abstracts over different LLM APIs (`OpenRouter`, `OpenAI`, etc.).
//! All providers must implement the same chat interface.

mod completions;
#[cfg(test)]
mod mock;
pub(crate) mod wire;

pub use completions::CompletionsProvider;
#[cfg(test)]
pub use mock::MockProvider;

use std::future::Future;

use crate::error::ProviderError;
use crate::types::{Message, Response, ToolDefinition};

/// A provider reply plus request-level metadata.
pub struct ChatOutcome {
    /// The model's reply.
    pub response: Response,
    /// Prompt size of the request as counted by the provider's
    /// tokenizer, when the API reports usage. Ground truth for
    /// context size — includes system prompt and tool schemas that
    /// char-based estimates miss.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "consumed by the context engine in the next commit"
        )
    )]
    pub prompt_tokens: Option<u32>,
}

/// LLM provider abstraction.
///
/// Implementors handle the specifics of communicating with different LLM APIs
/// (request format, authentication, parsing responses, etc.).
pub trait Provider: Send + Sync {
    /// Send messages to the LLM and get a response.
    ///
    /// # Arguments
    /// * `messages` - Conversation history (system, user, assistant, tool messages)
    /// * `tools` - Available tools the LLM can call
    ///
    /// # Returns
    /// Either a text response or tool call requests, with usage metadata.
    fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> impl Future<Output = Result<ChatOutcome, ProviderError>> + Send;
}
