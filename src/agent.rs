//! Core agent loop.
//!
//! Orchestrates the conversation between user, LLM, and tools.
//! Each turn sends context to the LLM and either returns a text response
//! or executes tool calls until the LLM completes.

use crate::error::Error;
use crate::provider::Provider;
use crate::tools::ToolRegistry;
use crate::types::{Message, Response};
use futures::future::join_all;

/// Maximum iterations per turn to prevent infinite loops.
const MAX_ITERATIONS: usize = 20;

/// Run a single turn of the agent loop.
///
/// # Arguments
/// * `user_message` - The user's input message
/// * `provider` - LLM provider for generating responses
/// * `tools` - Registry of available tools
///
/// # Returns
/// The final text response from the LLM
///
/// # Errors
/// Returns error if max iterations reached or provider fails
pub async fn run_turn<P: Provider>(
    user_message: &str,
    provider: &P,
    tools: &ToolRegistry,
) -> Result<String, Error> {
    let mut messages = vec![
        Message::System {
            content: build_system_prompt(),
        },
        Message::User {
            content: user_message.to_string(),
        },
    ];

    let tool_definitions = tools.definitions();

    for _iteration in 0..MAX_ITERATIONS {
        let response = provider
            .chat(&messages, &tool_definitions)
            .await
            .map_err(Error::Provider)?;

        match response {
            Response::Text(content) => {
                return Ok(content);
            }
            Response::ToolCalls(calls) => {
                messages.push(Message::Assistant {
                    content: String::new(),
                    tool_calls: Some(calls.clone()),
                });

                // Execute all tool calls in parallel
                let futures: Vec<_> = calls.iter().map(|call| tools.execute(call)).collect();
                let results = join_all(futures).await;

                // Add results to message history
                for (call, result) in calls.iter().zip(results) {
                    let content = result.unwrap_or_else(|e| format!("Error: {e}"));

                    messages.push(Message::Tool {
                        call_id: call.id.clone(),
                        content,
                    });
                }
            }
        }
    }

    Err(Error::MaxIterationsReached)
}

/// Build the system prompt.
///
/// Includes personality (SOUL.md), instructions (AGENTS.md), and context.
/// Currently returns a stub - will load from files later.
fn build_system_prompt() -> String {
    "You are Kitaebot, an autonomous agent running in a NixOS VM.".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ProviderError;
    use crate::tools::StubTool;
    use crate::types::{ToolCall, ToolDefinition, ToolFunction};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock provider that returns pre-configured responses.
    struct MockProvider {
        responses: Vec<Result<Response, ProviderError>>,
        call_count: Arc<AtomicUsize>,
    }

    impl MockProvider {
        fn new(responses: Vec<Result<Response, ProviderError>>) -> Self {
            Self {
                responses,
                call_count: Arc::new(AtomicUsize::new(0)),
            }
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

    fn text(s: &str) -> Response {
        Response::Text(s.to_string())
    }

    fn tool_call(id: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            call_type: "function".to_string(),
            function: ToolFunction {
                name: "stub".to_string(),
                arguments: "{}".to_string(),
            },
        }
    }

    fn tool_calls(ids: &[&str]) -> Response {
        Response::ToolCalls(ids.iter().map(|&id| tool_call(id)).collect())
    }

    fn tools_with_stub() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(StubTool));
        registry
    }

    #[tokio::test]
    async fn test_text_response() {
        let provider = MockProvider::new(vec![Ok(text("Hello from LLM"))]);

        let result = run_turn("Hello", &provider, &ToolRegistry::new()).await;
        assert_eq!(result.unwrap(), "Hello from LLM");
    }

    #[tokio::test]
    async fn test_tool_call_execution() {
        let provider = MockProvider::new(vec![
            Ok(tool_calls(&["call-1"])),
            Ok(text("Tool result processed")),
        ]);

        let result = run_turn("Use a tool", &provider, &tools_with_stub()).await;
        assert_eq!(result.unwrap(), "Tool result processed");
    }

    #[tokio::test]
    async fn test_max_iterations() {
        let provider = MockProvider::new(vec![Ok(tool_calls(&["call-infinite"])); MAX_ITERATIONS]);

        let result = run_turn("Infinite loop", &provider, &tools_with_stub()).await;
        assert!(matches!(result.unwrap_err(), Error::MaxIterationsReached));
    }

    #[tokio::test]
    async fn test_provider_error() {
        let provider =
            MockProvider::new(vec![Err(ProviderError::Network("Mock error".to_string()))]);

        let result = run_turn("Error case", &provider, &ToolRegistry::new()).await;
        assert!(matches!(result.unwrap_err(), Error::Provider(_)));
    }

    #[tokio::test]
    async fn test_parallel_tool_calls() {
        let provider = MockProvider::new(vec![
            Ok(tool_calls(&["call-1", "call-2"])),
            Ok(text("Multiple tools executed")),
        ]);

        let result = run_turn("Parallel tools", &provider, &tools_with_stub()).await;
        assert_eq!(result.unwrap(), "Multiple tools executed");
    }
}
