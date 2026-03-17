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
    fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> impl Future<Output = Result<Response, ProviderError>> + Send;
}
