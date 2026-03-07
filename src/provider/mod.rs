//! LLM provider abstraction.
//!
//! The Provider trait abstracts over different LLM APIs (`OpenRouter`, `OpenAI`, etc.).
//! All providers must implement the same chat interface.

#[cfg(test)]
mod mock;
mod openrouter;

#[cfg(test)]
pub use mock::MockProvider;
pub use openrouter::OpenRouterProvider;

use crate::error::ProviderError;
use crate::types::{Message, Response, ToolDefinition};

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
    /// Either a text response or tool call requests.
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<Response, ProviderError>;
}
