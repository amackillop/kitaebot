//! Core agent loop.
//!
//! Orchestrates the conversation between user, LLM, and tools.
//! Each turn sends context to the LLM and either returns a text response
//! or executes tool calls until the LLM completes.

use futures::future::join_all;
use tracing::{debug, error, warn};

use crate::config::ContextConfig;
use crate::context;
use crate::error::Error;
use crate::provider::Provider;
use crate::safety;
use crate::session::Session;
use crate::tools::Tools;
use crate::types::{Message, Response};

/// Static dependencies for an agent turn.
///
/// Bundles the provider, tools, iteration limit, and context config that
/// remain constant across turns, keeping `run_turn` signatures small.
pub struct TurnConfig<'a, P: Provider> {
    pub provider: &'a P,
    pub tools: &'a Tools,
    pub max_iterations: usize,
    pub context: &'a ContextConfig,
}

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
    config: &TurnConfig<'_, P>,
) -> Result<String, Error> {
    context::compact_if_needed(session, system_prompt, config.provider, config.context)
        .await
        .map_err(Error::Provider)?;

    session.add_message(Message::User {
        content: user_message.to_string(),
    });

    let tool_definitions = config.tools.definitions();

    for iteration in 0..config.max_iterations {
        debug!(iteration, "Agent loop iteration");
        // Prepend system prompt for each provider call (not stored in session)
        let mut messages = vec![Message::System {
            content: system_prompt.to_string(),
        }];
        messages.extend(session.messages().iter().cloned());

        let response = config
            .provider
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
                let futures: Vec<_> = calls
                    .iter()
                    .map(|call| config.tools.execute(call))
                    .collect();
                let results = join_all(futures).await;

                // Add results to session in the same order as tool_calls.
                // stats::analyze relies on this for positional correlation.
                for (call, result) in calls.iter().zip(results) {
                    let content = match result {
                        Ok(output) => {
                            match safety::check_tool_output(&call.function.name, &output) {
                                Ok(wrapped) => wrapped,
                                Err(e) => {
                                    warn!(
                                        tool = %call.function.name,
                                        "Tool output blocked: {e}",
                                    );
                                    format!("Tool output blocked: {e}. Do not retry.")
                                }
                            }
                        }
                        Err(e) => {
                            error!(
                                tool = %call.function.name,
                                "Tool execution failed: {e}",
                            );
                            format!("Error: {e}")
                        }
                    };

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
    use crate::config::ContextConfig;
    use crate::error::ProviderError;
    use crate::provider::MockProvider;
    use crate::tools::MockTool;
    use crate::types::{ToolCall, ToolFunction};

    fn text(s: &str) -> Response {
        Response::Text(s.to_string())
    }

    fn mock_call(id: &str) -> ToolCall {
        ToolCall::new(
            id.to_string(),
            ToolFunction {
                name: "mock".to_string(),
                arguments: "{}".to_string(),
            },
        )
    }

    fn mock_tool_calls(ids: &[&str]) -> Response {
        Response::ToolCalls {
            content: String::new(),
            calls: ids.iter().map(|&id| mock_call(id)).collect(),
        }
    }

    fn mock_tools(output: &str) -> Tools {
        Tools::new(vec![Box::new(MockTool::new(output))])
    }

    const SYSTEM: &str = "You are a test assistant.";
    const MAX_ITER: usize = 20;
    const CTX: ContextConfig = ContextConfig {
        max_tokens: 200_000,
        budget_percent: 80,
    };

    fn turn_config<'a>(
        provider: &'a MockProvider,
        tools: &'a Tools,
    ) -> TurnConfig<'a, MockProvider> {
        TurnConfig {
            provider,
            tools,
            max_iterations: MAX_ITER,
            context: &CTX,
        }
    }

    #[tokio::test]
    async fn test_text_response() {
        let provider = MockProvider::new(vec![Ok(text("Hello from LLM"))]);
        let tools = Tools::new(vec![]);
        let mut session = Session::new();

        let result = run_turn(
            &mut session,
            SYSTEM,
            "Hello",
            &turn_config(&provider, &tools),
        )
        .await;
        assert_eq!(result.unwrap(), "Hello from LLM");
        // User + Assistant messages stored
        assert_eq!(session.messages().len(), 2);
    }

    #[tokio::test]
    async fn test_tool_call_execution() {
        let provider = MockProvider::new(vec![
            Ok(mock_tool_calls(&["call-1"])),
            Ok(text("Tool result processed")),
        ]);
        let tools = mock_tools("mock output");
        let mut session = Session::new();

        let result = run_turn(
            &mut session,
            SYSTEM,
            "Use a tool",
            &turn_config(&provider, &tools),
        )
        .await;
        assert_eq!(result.unwrap(), "Tool result processed");
    }

    #[tokio::test]
    async fn test_max_iterations() {
        let provider = MockProvider::new(vec![Ok(mock_tool_calls(&["call-infinite"])); MAX_ITER]);
        let tools = mock_tools("mock output");
        let mut session = Session::new();

        let result = run_turn(
            &mut session,
            SYSTEM,
            "Infinite loop",
            &turn_config(&provider, &tools),
        )
        .await;
        assert!(matches!(result.unwrap_err(), Error::MaxIterationsReached));
    }

    #[tokio::test]
    async fn test_provider_error() {
        let provider =
            MockProvider::new(vec![Err(ProviderError::Network("Mock error".to_string()))]);
        let tools = Tools::new(vec![]);
        let mut session = Session::new();

        let result = run_turn(
            &mut session,
            SYSTEM,
            "Error case",
            &turn_config(&provider, &tools),
        )
        .await;
        assert!(matches!(result.unwrap_err(), Error::Provider(_)));
    }

    #[tokio::test]
    async fn test_parallel_tool_calls() {
        let provider = MockProvider::new(vec![
            Ok(mock_tool_calls(&["call-1", "call-2"])),
            Ok(text("Multiple tools executed")),
        ]);
        let tools = mock_tools("mock output");
        let mut session = Session::new();

        let result = run_turn(
            &mut session,
            SYSTEM,
            "Parallel tools",
            &turn_config(&provider, &tools),
        )
        .await;
        assert_eq!(result.unwrap(), "Multiple tools executed");
    }

    #[tokio::test]
    async fn test_safety_blocks_leaked_secret() {
        let provider = MockProvider::new(vec![
            Ok(mock_tool_calls(&["call-leak"])),
            Ok(text("Handled")),
        ]);
        let tools = mock_tools("Here is your key: sk-1234567890abcdef");
        let mut session = Session::new();

        let result = run_turn(
            &mut session,
            SYSTEM,
            "Leak test",
            &turn_config(&provider, &tools),
        )
        .await;
        assert_eq!(result.unwrap(), "Handled");

        // The tool message in session should contain the blocked message, not the secret
        let tool_msg = session
            .messages()
            .iter()
            .find(|m| matches!(m, Message::Tool { .. }))
            .expect("should have a tool message");

        if let Message::Tool { content, .. } = tool_msg {
            assert!(content.contains("Tool output blocked"));
            assert!(content.contains("Do not retry"));
            assert!(!content.contains("sk-1234567890abcdef"));
        }
    }

    #[tokio::test]
    async fn test_clean_tool_output_wrapped() {
        let provider = MockProvider::new(vec![Ok(mock_tool_calls(&["call-1"])), Ok(text("Done"))]);
        let tools = mock_tools("mock output");
        let mut session = Session::new();

        run_turn(
            &mut session,
            SYSTEM,
            "Wrap test",
            &turn_config(&provider, &tools),
        )
        .await
        .unwrap();

        let tool_msg = session
            .messages()
            .iter()
            .find(|m| matches!(m, Message::Tool { .. }))
            .expect("should have a tool message");

        if let Message::Tool { content, .. } = tool_msg {
            assert!(content.contains("<tool_output name=\"mock\">"));
            assert!(content.contains("</tool_output>"));
        }
    }
}
