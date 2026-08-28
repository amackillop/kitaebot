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
#[derive(Debug)]
pub struct ChatOutcome {
    /// The model's reply.
    pub response: Response,
    /// Usage as reported by the provider for this call.
    pub usage: CallUsage,
}

/// Per-call usage reported by the provider, when it reports any.
#[derive(Clone, Debug, Default)]
pub struct CallUsage {
    /// Prompt size as counted by the provider's tokenizer, when the API
    /// reports usage. Ground truth for context size — includes system
    /// prompt and tool schemas that char-based estimates miss.
    pub prompt_tokens: Option<u32>,
    /// Subset of `prompt_tokens` served from the provider's prompt
    /// cache. `None` when the response carried no
    /// `prompt_tokens_details`; `Some(0)` is a reported cold prompt.
    pub cached_tokens: Option<u32>,
    /// Upstream endpoint that served the call (`OpenRouter` names it
    /// in every response). Decides which endpoint's rates apply.
    pub provider: Option<String>,
    /// Tokens generated in the reply.
    pub completion_tokens: u32,
    /// Charged cost in USD; `OpenRouter` only, `None` elsewhere.
    pub cost: Option<f64>,
}

/// LLM provider abstraction.
///
/// Implementors handle the specifics of communicating with different LLM APIs
/// (request format, authentication, parsing responses, etc.).
pub trait Provider: Send + Sync {
    /// Send messages to the LLM and get a response.
    ///
    /// # Arguments
    /// * `session` - Stable identity of the conversation this call
    ///   belongs to. `OpenRouter` uses it as the sticky routing key
    ///   (`session_id`), pinning a session's requests to one upstream
    ///   replica so prompt-cache hits are deterministic rather than a
    ///   property of account-level hashing.
    /// * `messages` - Conversation history (system, user, assistant, tool messages)
    /// * `tools` - Available tools the LLM can call
    ///
    /// # Returns
    /// Either a text response or tool call requests, with usage metadata.
    fn chat(
        &self,
        session: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> impl Future<Output = Result<ChatOutcome, ProviderError>> + Send;

    /// The model this provider sends requests as; recorded per turn in
    /// the usage ledger.
    fn model(&self) -> &str;
}
