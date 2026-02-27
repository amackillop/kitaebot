//! Core agent loop.
//!
//! Orchestrates the conversation between user, LLM, and tools.
//! Each turn sends context to the LLM and either returns a text response
//! or executes tool calls until the LLM completes.

use crate::error::Error;
use crate::provider::Provider;
use crate::session::Session;
use crate::tools::Tools;
use crate::types::{Message, Response};
use futures::future::join_all;

/// Run a single turn of the agent loop.
///
/// Pushes the user message onto the session, sends the history (with system
/// prompt prepended) to the provider, and appends assistant/tool messages.
/// The system prompt is prepended per provider call but not stored in the
/// session, so edits to SOUL.md take effect without a restart.
///
/// # Errors
/// Returns error if max iterations reached or provider fails
pub async fn run_turn<P: Provider>(
    session: &mut Session,
    system_prompt: &str,
    user_message: &str,
    provider: &P,
    tools: &Tools,
    max_iterations: usize,
) -> Result<String, Error> {
    session.add_message(Message::User {
        content: user_message.to_string(),
    });

    let tool_definitions = tools.definitions();

    for _iteration in 0..max_iterations {
        // Prepend system prompt for each provider call (not stored in session)
        let mut messages = vec![Message::System {
            content: system_prompt.to_string(),
        }];
        messages.extend(session.messages().iter().cloned());

        let response = provider
            .chat(&messages, &tool_definitions)
            .await
            .map_err(Error::Provider)?;

        match response {
            Response::Text(content) => {
                session.add_message(Message::Assistant {
                    content: content.clone(),
                    tool_calls: None,
                });
                return Ok(content);
            }
            Response::ToolCalls { content, calls } => {
                session.add_message(Message::Assistant {
                    content,
                    tool_calls: Some(calls.clone()),
                });

                // Execute all tool calls in parallel
                let futures: Vec<_> = calls.iter().map(|call| tools.execute(call)).collect();
                let results = join_all(futures).await;

                // Add results to message history
                for (call, result) in calls.iter().zip(results) {
                    let content = result.unwrap_or_else(|e| format!("Error: {e}"));

                    session.add_message(Message::Tool {
                        call_id: call.id.clone(),
                        content,
                    });
                }
            }
        }
    }

    Err(Error::MaxIterationsReached)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ProviderError;
    use crate::provider::MockProvider;
    use crate::tools::{Stub, Tool};
    use crate::types::{ToolCall, ToolFunction};

    fn text(s: &str) -> Response {
        Response::Text(s.to_string())
    }

    fn tool_call(id: &str) -> ToolCall {
        ToolCall::new(
            id.to_string(),
            ToolFunction {
                name: "stub".to_string(),
                arguments: "{}".to_string(),
            },
        )
    }

    fn tool_calls(ids: &[&str]) -> Response {
        Response::ToolCalls {
            content: String::new(),
            calls: ids.iter().map(|&id| tool_call(id)).collect(),
        }
    }

    fn tools_with_stub() -> Tools {
        Tools::new(vec![Tool::Stub(Stub)])
    }

    const SYSTEM: &str = "You are a test assistant.";
    const MAX_ITER: usize = 20;

    #[tokio::test]
    async fn test_text_response() {
        let provider = MockProvider::new(vec![Ok(text("Hello from LLM"))]);
        let mut session = Session::new();

        let result = run_turn(
            &mut session,
            SYSTEM,
            "Hello",
            &provider,
            &Tools::new(vec![]),
            MAX_ITER,
        )
        .await;
        assert_eq!(result.unwrap(), "Hello from LLM");
        // User + Assistant messages stored
        assert_eq!(session.messages().len(), 2);
    }

    #[tokio::test]
    async fn test_tool_call_execution() {
        let provider = MockProvider::new(vec![
            Ok(tool_calls(&["call-1"])),
            Ok(text("Tool result processed")),
        ]);
        let mut session = Session::new();

        let result = run_turn(
            &mut session,
            SYSTEM,
            "Use a tool",
            &provider,
            &tools_with_stub(),
            MAX_ITER,
        )
        .await;
        assert_eq!(result.unwrap(), "Tool result processed");
    }

    #[tokio::test]
    async fn test_max_iterations() {
        let provider = MockProvider::new(vec![Ok(tool_calls(&["call-infinite"])); MAX_ITER]);
        let mut session = Session::new();

        let result = run_turn(
            &mut session,
            SYSTEM,
            "Infinite loop",
            &provider,
            &tools_with_stub(),
            MAX_ITER,
        )
        .await;
        assert!(matches!(result.unwrap_err(), Error::MaxIterationsReached));
    }

    #[tokio::test]
    async fn test_provider_error() {
        let provider =
            MockProvider::new(vec![Err(ProviderError::Network("Mock error".to_string()))]);
        let mut session = Session::new();

        let result = run_turn(
            &mut session,
            SYSTEM,
            "Error case",
            &provider,
            &Tools::new(vec![]),
            MAX_ITER,
        )
        .await;
        assert!(matches!(result.unwrap_err(), Error::Provider(_)));
    }

    #[tokio::test]
    async fn test_parallel_tool_calls() {
        let provider = MockProvider::new(vec![
            Ok(tool_calls(&["call-1", "call-2"])),
            Ok(text("Multiple tools executed")),
        ]);
        let mut session = Session::new();

        let result = run_turn(
            &mut session,
            SYSTEM,
            "Parallel tools",
            &provider,
            &tools_with_stub(),
            MAX_ITER,
        )
        .await;
        assert_eq!(result.unwrap(), "Multiple tools executed");
    }
}
