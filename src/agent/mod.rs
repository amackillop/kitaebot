//! Core agent loop.
//!
//! Orchestrates the conversation between user, LLM, and tools.
//! Each turn sends context to the LLM and either returns a text response
//! or executes tool calls until the LLM completes.

mod actor;
pub(crate) mod envelope;
mod handle;

pub use handle::AgentHandle;

use std::future::Future;
use std::path::Path;

use futures::future::join_all;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

use crate::activity::{self, Activity};
use crate::config::ContextConfig;
use crate::context;
use crate::error::{Error, ToolError};
use crate::provider::Provider;
use crate::safety;
use crate::session::Session;
use crate::tools::Tools;
use crate::types::{Message, Response, ToolCall};
use crate::workspace::Workspace;

/// Consecutive identical tool calls before execution is skipped.
const REPEAT_LIMIT: usize = 3;

const REPEAT_ERROR: &str = "ERROR: You have called this tool with identical \
    arguments multiple times and received the same result. \
    Do NOT retry the same call. Either use a different tool \
    or action, or respond to the user explaining what you \
    tried and why it did not work.";

/// Load session, run a single turn, and save regardless of outcome.
///
/// Shared by all channels (telegram, socket, heartbeat).
#[allow(clippy::too_many_arguments)]
pub async fn process_message<P: Provider>(
    session_path: &Path,
    workspace: &Workspace,
    user_message: &str,
    provider: &P,
    tools: &Tools,
    max_iterations: usize,
    ctx: ContextConfig,
    activity_tx: Option<&mpsc::Sender<Activity>>,
    cancel: &CancellationToken,
) -> Result<String, Error> {
    let mut session = Session::load(session_path)?;
    let system_prompt = workspace.system_prompt();
    let result = run_turn(
        &mut session,
        &system_prompt,
        user_message,
        provider,
        tools,
        max_iterations,
        ctx,
        activity_tx,
        cancel,
    )
    .await;
    session.save(session_path)?;
    result
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
#[allow(clippy::too_many_arguments)]
async fn run_turn<P: Provider>(
    session: &mut Session,
    system_prompt: &str,
    user_message: &str,
    provider: &P,
    tools: &Tools,
    max_iterations: usize,
    ctx: ContextConfig,
    activity_tx: Option<&mpsc::Sender<Activity>>,
    cancel: &CancellationToken,
) -> Result<String, Error> {
    if cancel.is_cancelled() {
        activity::emit(activity_tx, Activity::Cancelled);
        return Err(Error::Cancelled);
    }

    let before = context::session_tokens(session, system_prompt.len());
    let compact_fut = context::compact_if_needed(session, system_prompt, provider, ctx);
    let compacted = cancellable(compact_fut, cancel, activity_tx)
        .await?
        .map_err(Error::Provider)?;
    if compacted {
        let after = context::session_tokens(session, system_prompt.len());
        activity::emit(activity_tx, Activity::Compaction { before, after });
    }

    session.add_message(Message::User {
        content: user_message.to_string(),
    });

    let tool_definitions = tools.definitions();

    let mut repeats = RepeatDetector::new();

    for iteration in 0..max_iterations {
        if cancel.is_cancelled() {
            activity::emit(activity_tx, Activity::Cancelled);
            return Err(Error::Cancelled);
        }

        debug!(iteration, "Agent loop iteration");
        // Prepend system prompt for each provider call (not stored in session)
        let mut messages = vec![Message::System {
            content: system_prompt.to_string(),
        }];
        messages.extend(session.messages().iter().cloned());

        let response = cancellable(
            provider.chat(&messages, &tool_definitions),
            cancel,
            activity_tx,
        )
        .await?
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

                if repeats.record(&calls) {
                    warn!(
                        iteration,
                        "Repeated tool calls detected, skipping execution"
                    );
                    for call in &calls {
                        session.add_message(Message::Tool {
                            call_id: call.id.clone(),
                            content: REPEAT_ERROR.to_string(),
                        });
                    }
                    continue;
                }

                for call in &calls {
                    activity::emit(
                        activity_tx,
                        Activity::ToolStart {
                            tool: call.function.name.clone(),
                        },
                    );
                }

                // Execute all tool calls in parallel
                let futures: Vec<_> = calls.iter().map(|call| tools.execute(call)).collect();
                let results = cancellable(join_all(futures), cancel, activity_tx).await?;

                record_tool_results(session, &calls, results, activity_tx);
            }
        }
    }

    activity::emit(activity_tx, Activity::MaxIterations);
    Err(Error::MaxIterationsReached)
}

// ── Private helpers ─────────────────────────────────────────────────

/// Tracks consecutive identical tool call sets to detect stuck loops.
struct RepeatDetector {
    prev: Option<Vec<(String, serde_json::Value)>>,
    count: usize,
}

impl RepeatDetector {
    fn new() -> Self {
        Self {
            prev: None,
            count: 0,
        }
    }

    /// Record a new set of tool calls. Returns `true` if the limit is reached.
    fn record(&mut self, calls: &[ToolCall]) -> bool {
        let fingerprint: Vec<(String, serde_json::Value)> = calls
            .iter()
            .map(|c| {
                let args = serde_json::from_str(&c.function.arguments)
                    .unwrap_or_else(|_| serde_json::Value::String(c.function.arguments.clone()));
                (c.function.name.clone(), args)
            })
            .collect();

        if self.prev.as_ref() == Some(&fingerprint) {
            self.count += 1;
        } else {
            self.count = 1;
            self.prev = Some(fingerprint);
        }

        self.count >= REPEAT_LIMIT
    }
}

/// Race a future against a cancellation token.
///
/// Returns the future's output on completion, or `Err(Cancelled)` if the
/// token fires first. Emits `Activity::Cancelled` before returning.
async fn cancellable<T>(
    future: impl Future<Output = T>,
    cancel: &CancellationToken,
    activity_tx: Option<&mpsc::Sender<Activity>>,
) -> Result<T, Error> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => {
            activity::emit(activity_tx, Activity::Cancelled);
            Err(Error::Cancelled)
        }
        output = future => Ok(output),
    }
}

/// Process tool execution results: check safety, emit events, record to session.
fn record_tool_results(
    session: &mut Session,
    calls: &[ToolCall],
    results: Vec<Result<String, ToolError>>,
    activity_tx: Option<&mpsc::Sender<Activity>>,
) {
    for (call, result) in calls.iter().zip(results) {
        let (content, err) = match result {
            Ok(output) => match safety::check_tool_output(&call.function.name, &output) {
                Ok(wrapped) => (wrapped, None),
                Err(e) => {
                    warn!(tool = %call.function.name, "Tool output blocked: {e}");
                    let msg = format!("Tool output blocked: {e}. Do not retry.");
                    (msg, Some(e.to_string()))
                }
            },
            Err(e) => {
                error!(tool = %call.function.name, "Tool execution failed: {e}");
                (format!("Error: {e}"), Some(e.to_string()))
            }
        };

        activity::emit(
            activity_tx,
            Activity::ToolEnd {
                tool: call.function.name.clone(),
                error: err,
            },
        );

        session.add_message(Message::Tool {
            call_id: call.id.clone(),
            content,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ContextConfig;
    use crate::error::ProviderError;
    use crate::provider::MockProvider;
    use crate::tools::MockTool;
    use crate::types::{ToolCall, ToolFunction};

    fn noop_cancel() -> CancellationToken {
        CancellationToken::new()
    }

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
        Tools::new(vec![Box::new(MockTool::new(output))], &[]).unwrap()
    }

    const SYSTEM: &str = "You are a test assistant.";
    const MAX_ITER: usize = 20;
    const CTX: ContextConfig = ContextConfig {
        max_tokens: 200_000,
        budget_percent: 80,
    };

    #[tokio::test]
    async fn test_text_response() {
        let provider = MockProvider::new(vec![Ok(text("Hello from LLM"))]);
        let tools = Tools::default();
        let mut session = Session::new();

        let result = run_turn(
            &mut session,
            SYSTEM,
            "Hello",
            &provider,
            &tools,
            MAX_ITER,
            CTX,
            None,
            &noop_cancel(),
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
            &provider,
            &tools,
            MAX_ITER,
            CTX,
            None,
            &noop_cancel(),
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
            &provider,
            &tools,
            MAX_ITER,
            CTX,
            None,
            &noop_cancel(),
        )
        .await;
        assert!(matches!(result.unwrap_err(), Error::MaxIterationsReached));
    }

    #[tokio::test]
    async fn test_repeated_tool_calls_skipped() {
        // Provider returns the same tool call 5 times, then text.
        // With REPEAT_LIMIT=3, calls 1-2 execute normally, 3-5 are skipped.
        let provider = MockProvider::new(vec![
            Ok(mock_tool_calls(&["c1"])),
            Ok(mock_tool_calls(&["c2"])),
            Ok(mock_tool_calls(&["c3"])),
            Ok(mock_tool_calls(&["c4"])),
            Ok(mock_tool_calls(&["c5"])),
            Ok(text("Gave up")),
        ]);
        let tools = mock_tools("same output");
        let mut session = Session::new();
        let (tx, mut rx) = mpsc::channel(64);

        let result = run_turn(
            &mut session,
            SYSTEM,
            "Loop test",
            &provider,
            &tools,
            MAX_ITER,
            CTX,
            Some(&tx),
            &noop_cancel(),
        )
        .await;

        assert_eq!(result.unwrap(), "Gave up");
        // Provider called 6 times (5 tool-call responses + 1 text).
        assert_eq!(provider.call_count(), 6);

        // Only iterations 1 and 2 should have emitted ToolStart/ToolEnd events.
        // Iterations 3-5 are skipped (no execution, no activity events).
        drop(tx);
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        let tool_starts = events
            .iter()
            .filter(|e| matches!(e, Activity::ToolStart { .. }))
            .count();
        let tool_ends = events
            .iter()
            .filter(|e| matches!(e, Activity::ToolEnd { .. }))
            .count();
        assert_eq!(tool_starts, 2);
        assert_eq!(tool_ends, 2);

        // Session should contain the repetition error message for skipped calls.
        let repetition_msgs: Vec<_> = session
            .messages()
            .iter()
            .filter(|m| matches!(m, Message::Tool { content, .. } if content.starts_with("ERROR: You have called")))
            .collect();
        assert_eq!(repetition_msgs.len(), 3); // iterations 3, 4, 5
    }

    #[tokio::test]
    async fn test_different_tool_calls_not_flagged() {
        // Different arguments each time — no repetition detected.
        let call_a = Response::ToolCalls {
            content: String::new(),
            calls: vec![ToolCall::new(
                "c1".to_string(),
                ToolFunction {
                    name: "mock".to_string(),
                    arguments: r#"{"x":1}"#.to_string(),
                },
            )],
        };
        let call_b = Response::ToolCalls {
            content: String::new(),
            calls: vec![ToolCall::new(
                "c2".to_string(),
                ToolFunction {
                    name: "mock".to_string(),
                    arguments: r#"{"x":2}"#.to_string(),
                },
            )],
        };
        let provider = MockProvider::new(vec![Ok(call_a), Ok(call_b), Ok(text("Done"))]);
        let tools = mock_tools("output");
        let mut session = Session::new();

        let result = run_turn(
            &mut session,
            SYSTEM,
            "No repeat",
            &provider,
            &tools,
            MAX_ITER,
            CTX,
            None,
            &noop_cancel(),
        )
        .await;
        assert_eq!(result.unwrap(), "Done");

        // No repetition error messages.
        let repetition_msgs: Vec<_> = session
            .messages()
            .iter()
            .filter(|m| matches!(m, Message::Tool { content, .. } if content.starts_with("ERROR: You have called")))
            .collect();
        assert!(repetition_msgs.is_empty());
    }

    #[tokio::test]
    async fn test_repeat_counter_resets_on_different_call() {
        // A, A, B, B, B — only B triggers the limit, not A.
        let call_a = || Response::ToolCalls {
            content: String::new(),
            calls: vec![ToolCall::new(
                "id".to_string(),
                ToolFunction {
                    name: "mock".to_string(),
                    arguments: r#"{"v":"a"}"#.to_string(),
                },
            )],
        };
        let call_b = || Response::ToolCalls {
            content: String::new(),
            calls: vec![ToolCall::new(
                "id".to_string(),
                ToolFunction {
                    name: "mock".to_string(),
                    arguments: r#"{"v":"b"}"#.to_string(),
                },
            )],
        };
        let provider = MockProvider::new(vec![
            Ok(call_a()),
            Ok(call_a()), // repeat_count=2 for A
            Ok(call_b()), // reset to 1 for B
            Ok(call_b()), // repeat_count=2 for B
            Ok(call_b()), // repeat_count=3 → skipped
            Ok(text("Done")),
        ]);
        let tools = mock_tools("output");
        let mut session = Session::new();
        let (tx, mut rx) = mpsc::channel(64);

        let result = run_turn(
            &mut session,
            SYSTEM,
            "Reset test",
            &provider,
            &tools,
            MAX_ITER,
            CTX,
            Some(&tx),
            &noop_cancel(),
        )
        .await;
        assert_eq!(result.unwrap(), "Done");

        // 4 executed iterations (A, A, B, B) + 1 skipped (B) = 4 ToolStart events
        drop(tx);
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        let tool_starts = events
            .iter()
            .filter(|e| matches!(e, Activity::ToolStart { .. }))
            .count();
        assert_eq!(tool_starts, 4);
    }

    #[tokio::test]
    async fn test_provider_error() {
        let provider =
            MockProvider::new(vec![Err(ProviderError::Network("Mock error".to_string()))]);
        let tools = Tools::default();
        let mut session = Session::new();

        let result = run_turn(
            &mut session,
            SYSTEM,
            "Error case",
            &provider,
            &tools,
            MAX_ITER,
            CTX,
            None,
            &noop_cancel(),
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
            &provider,
            &tools,
            MAX_ITER,
            CTX,
            None,
            &noop_cancel(),
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
        let tools = mock_tools("Here is your key: sk-proj-abc123def456ghi789jkl012");
        let mut session = Session::new();

        let result = run_turn(
            &mut session,
            SYSTEM,
            "Leak test",
            &provider,
            &tools,
            MAX_ITER,
            CTX,
            None,
            &noop_cancel(),
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
            assert!(!content.contains("sk-proj-abc123def456ghi789jkl012"));
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
            &provider,
            &tools,
            MAX_ITER,
            CTX,
            None,
            &noop_cancel(),
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

    #[tokio::test]
    async fn test_activity_tool_events() {
        let provider = MockProvider::new(vec![
            Ok(mock_tool_calls(&["call-1", "call-2"])),
            Ok(text("Done")),
        ]);
        let tools = mock_tools("mock output");
        let mut session = Session::new();
        let (tx, mut rx) = mpsc::channel(64);

        run_turn(
            &mut session,
            SYSTEM,
            "Activity test",
            &provider,
            &tools,
            MAX_ITER,
            CTX,
            Some(&tx),
            &noop_cancel(),
        )
        .await
        .unwrap();

        drop(tx);
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }

        // 2 ToolStart + 2 ToolEnd = 4 events
        assert_eq!(events.len(), 4);
        assert!(matches!(&events[0], Activity::ToolStart { tool } if tool == "mock"));
        assert!(matches!(&events[1], Activity::ToolStart { tool } if tool == "mock"));
        assert!(matches!(&events[2], Activity::ToolEnd { tool, error: None } if tool == "mock"));
        assert!(matches!(&events[3], Activity::ToolEnd { tool, error: None } if tool == "mock"));
    }

    #[tokio::test]
    async fn test_activity_max_iterations() {
        let provider = MockProvider::new(vec![Ok(mock_tool_calls(&["call-inf"])); MAX_ITER]);
        let tools = mock_tools("mock output");
        let mut session = Session::new();
        let (tx, mut rx) = mpsc::channel(256);

        let _ = run_turn(
            &mut session,
            SYSTEM,
            "Max iter activity",
            &provider,
            &tools,
            MAX_ITER,
            CTX,
            Some(&tx),
            &noop_cancel(),
        )
        .await;

        drop(tx);
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }

        // Last event should be MaxIterations
        assert!(matches!(events.last().unwrap(), Activity::MaxIterations));
    }

    #[tokio::test]
    async fn test_pre_cancelled_token_returns_cancelled() {
        let provider = MockProvider::new(vec![]);
        let tools = Tools::default();
        let mut session = Session::new();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = run_turn(
            &mut session,
            SYSTEM,
            "Should not run",
            &provider,
            &tools,
            MAX_ITER,
            CTX,
            None,
            &cancel,
        )
        .await;
        assert!(matches!(result.unwrap_err(), Error::Cancelled));
        assert_eq!(provider.call_count(), 0);
    }

    #[tokio::test]
    async fn test_process_message_saves_session_on_provider_error() {
        use crate::workspace::Workspace;

        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::init_at(dir.path().to_path_buf()).unwrap();
        let session_path = dir.path().join("sessions").join("test.json");

        let provider = MockProvider::new(vec![Err(ProviderError::Network(
            "connection refused".into(),
        ))]);
        let tools = Tools::default();

        let result = process_message(
            &session_path,
            &workspace,
            "Hello?",
            &provider,
            &tools,
            MAX_ITER,
            CTX,
            None,
            &noop_cancel(),
        )
        .await;
        assert!(result.is_err());

        let saved = Session::load(&session_path).unwrap();
        assert_eq!(saved.messages().len(), 1);
        assert!(matches!(
            &saved.messages()[0],
            Message::User { content } if content == "Hello?"
        ));
    }
}
