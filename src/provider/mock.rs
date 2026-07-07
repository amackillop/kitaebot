//! Mock provider for tests.
//!
//! Returns pre-configured responses in order, tracking call count.
//! Shared across `agent` and `heartbeat` test modules.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::error::ProviderError;
use crate::provider::{ChatOutcome, Provider};
use crate::types::{Message, Response, ToolDefinition};

/// Mock provider that returns pre-configured responses in sequence.
pub struct MockProvider {
    responses: Vec<Result<Response, ProviderError>>,
    prompt_tokens: Option<u32>,
    call_count: Arc<AtomicUsize>,
}

impl MockProvider {
    pub fn new(responses: Vec<Result<Response, ProviderError>>) -> Self {
        Self {
            responses,
            prompt_tokens: None,
            call_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Attach a `prompt_tokens` value to every successful response.
    pub fn with_prompt_tokens(mut self, tokens: u32) -> Self {
        self.prompt_tokens = Some(tokens);
        self
    }

    pub fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }
}

impl Provider for MockProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
    ) -> Result<ChatOutcome, ProviderError> {
        let index = self.call_count.fetch_add(1, Ordering::SeqCst);
        self.responses[index].clone().map(|response| ChatOutcome {
            response,
            prompt_tokens: self.prompt_tokens,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn prompt_tokens_default_none() {
        let provider = MockProvider::new(vec![Ok(Response::Text("hi".to_string()))]);
        let outcome = provider.chat(&[], &[]).await.unwrap();
        assert_eq!(outcome.prompt_tokens, None);
    }

    #[tokio::test]
    async fn with_prompt_tokens_attaches_value() {
        let provider =
            MockProvider::new(vec![Ok(Response::Text("hi".to_string()))]).with_prompt_tokens(1234);
        let outcome = provider.chat(&[], &[]).await.unwrap();
        assert_eq!(outcome.prompt_tokens, Some(1234));
    }
}
