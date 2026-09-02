//! Mock provider for tests.
//!
//! Returns pre-configured responses in order, tracking call count.
//! Shared across the agent, command, and channel test modules.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Semaphore;

use crate::error::ProviderError;
use crate::provider::{CallUsage, ChatError, ChatOutcome, Provider};
use crate::types::{Message, Response, ToolDefinition};

/// Mock provider that returns pre-configured responses in sequence.
pub struct MockProvider {
    responses: Vec<Result<Response, ProviderError>>,
    usage: CallUsage,
    /// Failed-draw usage attached to every reply and error.
    failed: Vec<CallUsage>,
    call_count: Arc<AtomicUsize>,
    /// Messages of the most recent `chat` call, for request assertions.
    last_messages: Arc<Mutex<Vec<Message>>>,
    /// Tool definitions of every `chat` call, in call order.
    requests_tools: Arc<Mutex<Vec<Vec<ToolDefinition>>>>,
    /// Each `chat` call takes one permit first, so a test can hold a
    /// turn in flight.
    gate: Option<Arc<Semaphore>>,
}

impl MockProvider {
    pub fn new(responses: Vec<Result<Response, ProviderError>>) -> Self {
        Self {
            responses,
            usage: CallUsage::default(),
            failed: Vec::new(),
            call_count: Arc::new(AtomicUsize::new(0)),
            last_messages: Arc::new(Mutex::new(Vec::new())),
            requests_tools: Arc::new(Mutex::new(Vec::new())),
            gate: None,
        }
    }

    /// Block every `chat` call until the test adds a permit to `gate`.
    pub fn gated(mut self, gate: Arc<Semaphore>) -> Self {
        self.gate = Some(gate);
        self
    }

    /// Attach a `prompt_tokens` value to every successful response.
    pub fn with_prompt_tokens(mut self, tokens: u32) -> Self {
        self.usage.prompt_tokens = Some(tokens);
        self
    }

    /// Attach failed-draw usage to every reply and error, as if each
    /// chat had burned these draws before resolving.
    pub fn with_failed_usage(mut self, failed: Vec<CallUsage>) -> Self {
        self.failed = failed;
        self
    }

    pub fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }

    /// The messages sent to the most recent `chat` call, or `None`
    /// before the first call.
    pub fn last_request(&self) -> Option<Vec<Message>> {
        let held = self
            .last_messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (!held.is_empty()).then(|| held.clone())
    }

    /// The tool definitions sent with the `index`th `chat` call.
    pub fn request_tools(&self, index: usize) -> Vec<ToolDefinition> {
        self.requests_tools
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(index)
            .cloned()
            .unwrap_or_default()
    }
}

impl Provider for MockProvider {
    async fn chat(
        &self,
        _session: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatOutcome, ChatError> {
        let index = self.call_count.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.gate {
            gate.acquire().await.expect("gate never closed").forget();
        }
        *self
            .last_messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = messages.to_vec();
        self.requests_tools
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(tools.to_vec());
        match self.responses[index].clone() {
            Ok(response) => Ok(ChatOutcome {
                response,
                usage: self.usage.clone(),
                failed: self.failed.clone(),
            }),
            Err(error) => Err(ChatError {
                error,
                failed: self.failed.clone(),
            }),
        }
    }

    #[allow(clippy::unnecessary_literal_bound)] // trait ties the return to &self
    fn model(&self) -> &str {
        "mock"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn prompt_tokens_default_none() {
        let provider = MockProvider::new(vec![Ok(Response::Text("hi".to_string()))]);
        let outcome = provider.chat("s", &[], &[]).await.unwrap();
        assert_eq!(outcome.usage.prompt_tokens, None);
    }

    #[tokio::test]
    async fn with_prompt_tokens_attaches_value() {
        let provider =
            MockProvider::new(vec![Ok(Response::Text("hi".to_string()))]).with_prompt_tokens(1234);
        let outcome = provider.chat("s", &[], &[]).await.unwrap();
        assert_eq!(outcome.usage.prompt_tokens, Some(1234));
    }
}
