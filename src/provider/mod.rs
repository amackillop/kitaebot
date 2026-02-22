//! LLM provider abstraction.
//!
//! The Provider trait abstracts over different LLM APIs (`OpenRouter`, `OpenAI`, etc.).
//! All providers must implement the same chat interface.

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

/// Stub provider for testing.
///
/// Returns a fixed response without making any API calls.
/// Used for testing the agent loop before implementing the real `OpenRouter` client.
pub struct StubProvider;

impl Provider for StubProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
    ) -> Result<Response, ProviderError> {
        Ok(Response::Text(
            "This is a stub response. Implement OpenRouter provider to get real responses."
                .to_string(),
        ))
    }
}
