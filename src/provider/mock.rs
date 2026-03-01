//! Mock provider for tests.
//!
//! Returns pre-configured responses in order, tracking call count.
//! Shared across `agent` and `heartbeat` test modules.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::error::ProviderError;
use crate::provider::Provider;
use crate::types::{Message, Response, ToolDefinition};

/// Mock provider that returns pre-configured responses in sequence.
pub struct MockProvider {
    responses: Vec<Result<Response, ProviderError>>,
    call_count: Arc<AtomicUsize>,
}

impl MockProvider {
    pub fn new(responses: Vec<Result<Response, ProviderError>>) -> Self {
        Self {
            responses,
            call_count: Arc::new(AtomicUsize::new(0)),
        }
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
    ) -> Result<Response, ProviderError> {
        let index = self.call_count.fetch_add(1, Ordering::SeqCst);
        self.responses[index].clone()
    }
}
