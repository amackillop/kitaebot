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

use futures::future::join_all;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

use crate::activity::{self, Activity};
use crate::engine::{ContextEngine, SummarizeFn};
use crate::error::{Error, ToolError};
use crate::provider::Provider;
use crate::safety;
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

/// Maximum policy violations (Blocked errors) before the turn is halted.
const POLICY_STRIKE_LIMIT: usize = 2;

const POLICY_STOP_DIRECTIVE: &str = "POLICY VIOLATION: A tool call was blocked. \
    Do NOT work around this. Report the situation to the user and await direction.";

fn policy_halt_msg(reasons: &[String]) -> String {
    use std::fmt::Write;
    let mut msg = String::from(
        "I attempted to use a blocked operation multiple times. \
         The turn was halted automatically.",
    );
    if !reasons.is_empty() {
        let _ = write!(msg, " Blocked: {}", reasons.join("; "));
    }
    msg.push_str(" Please advise how to proceed.");
    msg
}

/// Run a single turn: compact if needed, push user message, loop until done.
///
/// Shared by all channels (telegram, socket, heartbeat).
#[allow(clippy::too_many_arguments)]
pub async fn process_message(
    engine: &mut impl ContextEngine,
    summarize: &SummarizeFn,
    workspace: &Workspace,
    user_message: &str,
    provider: &impl Provider,
    tools: &Tools,
    max_iterations: usize,
    activity_tx: Option<&mpsc::Sender<Activity>>,
    cancel: &CancellationToken,
) -> Result<String, Error> {
    let system_prompt = workspace.system_prompt();
    run_turn(
        engine,
        summarize,
        &system_prompt,
        user_message,
        provider,
        tools,
        max_iterations,
        activity_tx,
        cancel,
    )
    .await
}

/// Run a single turn of the agent loop.
///
/// Pushes the user message onto the session, sends the history (with system
/// prompt prepended) to the provider, and appends assistant/tool messages.
/// The system prompt is assembled per provider call via `engine.assemble()`,
/// so edits to SOUL.md take effect without a restart.
///
/// # Errors
/// Returns error if max iterations reached or provider fails
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn run_turn(
    engine: &mut impl ContextEngine,
    summarize: &SummarizeFn,
    system_prompt: &str,
    user_message: &str,
    provider: &impl Provider,
    tools: &Tools,
    max_iterations: usize,
    activity_tx: Option<&mpsc::Sender<Activity>>,
    cancel: &CancellationToken,
) -> Result<String, Error> {
    if cancel.is_cancelled() {
        activity::emit(activity_tx, Activity::Cancelled);
        return Err(Error::Cancelled);
    }

    let before = engine.stats().token_estimate;
    if let Some(event) = engine.compact_if_needed(summarize).await? {
        activity::emit(
            activity_tx,
            Activity::Compaction {
                before: event.before,
                after: event.after,
            },
        );
        let _ = before; // used only for the "did we compact?" check
    }

    engine
        .push_message(Message::User {
            content: user_message.to_string(),
        })
        .await?;

    let tool_definitions = tools.definitions();

    let mut repeats = RepeatDetector::new();
    let mut policy_strikes: usize = 0;

    for iteration in 0..max_iterations {
        if cancel.is_cancelled() {
            activity::emit(activity_tx, Activity::Cancelled);
            return Err(Error::Cancelled);
        }

        debug!(iteration, "Agent loop iteration");
        let assembled = engine.assemble(system_prompt).await?;

        let response = cancellable(
            provider.chat(&assembled.messages, &tool_definitions),
            cancel,
            activity_tx,
        )
        .await?
        .map_err(Error::Provider)?;

        match response {
            Response::Text(content) => {
                engine
                    .push_message(Message::Assistant {
                        content: content.clone(),
                    })
                    .await?;
                return Ok(content);
            }
            Response::ToolCalls { content, calls } => {
                engine
                    .push_message(Message::ToolCalls {
                        content,
                        calls: calls.clone(),
                    })
                    .await?;

                if repeats.record(&calls) {
                    warn!(
                        iteration,
                        "Repeated tool calls detected, skipping execution"
                    );
                    for call in &calls {
                        engine
                            .push_message(Message::Tool {
                                call_id: call.id.clone(),
                                content: REPEAT_ERROR.to_string(),
                            })
                            .await?;
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

                let blocked_reasons: Vec<String> = results
                    .iter()
                    .filter_map(|r| match r {
                        Err(ToolError::Blocked {
                            operation,
                            guidance,
                        }) => Some(format!("{operation} ({guidance})")),
                        _ => None,
                    })
                    .collect();

                record_tool_results(engine, &calls, results, activity_tx).await;

                if !blocked_reasons.is_empty() {
                    policy_strikes += 1;
                    if policy_strikes >= POLICY_STRIKE_LIMIT {
                        warn!("Policy strike limit reached, halting turn");
                        return Ok(policy_halt_msg(&blocked_reasons));
                    }
                    engine
                        .push_message(Message::System {
                            content: POLICY_STOP_DIRECTIVE.to_string(),
                        })
                        .await?;
                }
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

/// Process tool execution results: check safety, emit events, record to engine.
async fn record_tool_results<E: ContextEngine>(
    engine: &mut E,
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

        // Ignore push_message errors in tool result recording -- the turn
        // will fail on the next assemble() call if the engine is broken.
        let _ = engine
            .push_message(Message::Tool {
                call_id: call.id.clone(),
                content,
            })
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ContextConfig;
    use crate::engine::flat::FlatSession;
    use crate::engine::make_summarize_fn;
    use crate::error::ProviderError;
    use crate::provider::MockProvider;
    use crate::tools::{MockBlockedTool, MockTool};
    use crate::types::{ToolCall, ToolFunction};
    use std::sync::Arc;

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

    fn test_engine() -> FlatSession {
        let dir = tempfile::tempdir().unwrap();
        #[allow(deprecated)]
        let path = dir.into_path().join("session.json");
        FlatSession::new(path, ContextConfig::default()).unwrap()
    }

    fn test_summarize(provider: &Arc<MockProvider>) -> SummarizeFn {
        make_summarize_fn(provider.clone())
    }

    #[tokio::test]
    async fn test_text_response() {
        let provider = Arc::new(MockProvider::new(vec![Ok(text("Hello from LLM"))]));
        let tools = Tools::default();
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        let result = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Hello",
            &*provider,
            &tools,
            MAX_ITER,
            None,
            &noop_cancel(),
        )
        .await;
        assert_eq!(result.unwrap(), "Hello from LLM");
        // User + Assistant messages stored
        assert_eq!(engine.stats().message_count, 2);
    }

    #[tokio::test]
    async fn test_tool_call_execution() {
        let provider = Arc::new(MockProvider::new(vec![
            Ok(mock_tool_calls(&["call-1"])),
            Ok(text("Tool result processed")),
        ]));
        let tools = mock_tools("mock output");
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        let result = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Use a tool",
            &*provider,
            &tools,
            MAX_ITER,
            None,
            &noop_cancel(),
        )
        .await;
        assert_eq!(result.unwrap(), "Tool result processed");
    }

    #[tokio::test]
    async fn test_max_iterations() {
        let provider = Arc::new(MockProvider::new(vec![
            Ok(mock_tool_calls(&[
                "call-infinite"
            ]));
            MAX_ITER
        ]));
        let tools = mock_tools("mock output");
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        let result = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Infinite loop",
            &*provider,
            &tools,
            MAX_ITER,
            None,
            &noop_cancel(),
        )
        .await;
        assert!(matches!(result.unwrap_err(), Error::MaxIterationsReached));
    }

    #[tokio::test]
    async fn test_repeated_tool_calls_skipped() {
        let provider = Arc::new(MockProvider::new(vec![
            Ok(mock_tool_calls(&["c1"])),
            Ok(mock_tool_calls(&["c2"])),
            Ok(mock_tool_calls(&["c3"])),
            Ok(mock_tool_calls(&["c4"])),
            Ok(mock_tool_calls(&["c5"])),
            Ok(text("Gave up")),
        ]));
        let tools = mock_tools("same output");
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);
        let (tx, mut rx) = mpsc::channel(64);

        let result = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Loop test",
            &*provider,
            &tools,
            MAX_ITER,
            Some(&tx),
            &noop_cancel(),
        )
        .await;

        assert_eq!(result.unwrap(), "Gave up");
        assert_eq!(provider.call_count(), 6);

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

        // Assembled context should contain repetition error messages for skipped calls.
        let ctx = engine.assemble(SYSTEM).await.unwrap();
        let repetition_msgs: Vec<_> = ctx
            .messages
            .iter()
            .filter(|m| {
                matches!(m, Message::Tool { content, .. } if content.starts_with("ERROR: You have called"))
            })
            .collect();
        assert_eq!(repetition_msgs.len(), 3); // iterations 3, 4, 5
    }

    #[tokio::test]
    async fn test_different_tool_calls_not_flagged() {
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
        let provider = Arc::new(MockProvider::new(vec![
            Ok(call_a),
            Ok(call_b),
            Ok(text("Done")),
        ]));
        let tools = mock_tools("output");
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        let result = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "No repeat",
            &*provider,
            &tools,
            MAX_ITER,
            None,
            &noop_cancel(),
        )
        .await;
        assert_eq!(result.unwrap(), "Done");

        // No repetition error messages in assembled context.
        let ctx = engine.assemble(SYSTEM).await.unwrap();
        let repetition_msgs: Vec<_> = ctx
            .messages
            .iter()
            .filter(|m| {
                matches!(m, Message::Tool { content, .. } if content.starts_with("ERROR: You have called"))
            })
            .collect();
        assert!(repetition_msgs.is_empty());
    }

    #[tokio::test]
    async fn test_repeat_counter_resets_on_different_call() {
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
        let provider = Arc::new(MockProvider::new(vec![
            Ok(call_a()),
            Ok(call_a()),
            Ok(call_b()),
            Ok(call_b()),
            Ok(call_b()),
            Ok(text("Done")),
        ]));
        let tools = mock_tools("output");
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);
        let (tx, mut rx) = mpsc::channel(64);

        let result = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Reset test",
            &*provider,
            &tools,
            MAX_ITER,
            Some(&tx),
            &noop_cancel(),
        )
        .await;
        assert_eq!(result.unwrap(), "Done");

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
        let provider = Arc::new(MockProvider::new(vec![Err(ProviderError::Network(
            "Mock error".to_string(),
        ))]));
        let tools = Tools::default();
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        let result = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Error case",
            &*provider,
            &tools,
            MAX_ITER,
            None,
            &noop_cancel(),
        )
        .await;
        assert!(matches!(result.unwrap_err(), Error::Provider(_)));
    }

    #[tokio::test]
    async fn test_parallel_tool_calls() {
        let provider = Arc::new(MockProvider::new(vec![
            Ok(mock_tool_calls(&["call-1", "call-2"])),
            Ok(text("Multiple tools executed")),
        ]));
        let tools = mock_tools("mock output");
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        let result = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Parallel tools",
            &*provider,
            &tools,
            MAX_ITER,
            None,
            &noop_cancel(),
        )
        .await;
        assert_eq!(result.unwrap(), "Multiple tools executed");
    }

    #[tokio::test]
    async fn test_safety_blocks_leaked_secret() {
        let provider = Arc::new(MockProvider::new(vec![
            Ok(mock_tool_calls(&["call-leak"])),
            Ok(text("Handled")),
        ]));
        let tools = mock_tools("Here is your key: sk-proj-abc123def456ghi789jkl012");
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Leak test",
            &*provider,
            &tools,
            MAX_ITER,
            None,
            &noop_cancel(),
        )
        .await
        .unwrap();

        // Assemble to inspect messages (system prompt + session messages)
        let ctx = engine.assemble("").await.unwrap();
        let tool_msg = ctx
            .messages
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
        let provider = Arc::new(MockProvider::new(vec![
            Ok(mock_tool_calls(&["call-1"])),
            Ok(text("Done")),
        ]));
        let tools = mock_tools("mock output");
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Wrap test",
            &*provider,
            &tools,
            MAX_ITER,
            None,
            &noop_cancel(),
        )
        .await
        .unwrap();

        let ctx = engine.assemble("").await.unwrap();
        let tool_msg = ctx
            .messages
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
        let provider = Arc::new(MockProvider::new(vec![
            Ok(mock_tool_calls(&["call-1", "call-2"])),
            Ok(text("Done")),
        ]));
        let tools = mock_tools("mock output");
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);
        let (tx, mut rx) = mpsc::channel(64);

        run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Activity test",
            &*provider,
            &tools,
            MAX_ITER,
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

        assert_eq!(events.len(), 4);
        assert!(matches!(&events[0], Activity::ToolStart { tool } if tool == "mock"));
        assert!(matches!(&events[1], Activity::ToolStart { tool } if tool == "mock"));
        assert!(matches!(&events[2], Activity::ToolEnd { tool, error: None } if tool == "mock"));
        assert!(matches!(&events[3], Activity::ToolEnd { tool, error: None } if tool == "mock"));
    }

    #[tokio::test]
    async fn test_activity_max_iterations() {
        let provider = Arc::new(MockProvider::new(vec![
            Ok(mock_tool_calls(&["call-inf"]));
            MAX_ITER
        ]));
        let tools = mock_tools("mock output");
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);
        let (tx, mut rx) = mpsc::channel(256);

        let _ = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Max iter activity",
            &*provider,
            &tools,
            MAX_ITER,
            Some(&tx),
            &noop_cancel(),
        )
        .await;

        drop(tx);
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }

        assert!(matches!(events.last().unwrap(), Activity::MaxIterations));
    }

    #[tokio::test]
    async fn test_pre_cancelled_token_returns_cancelled() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let tools = Tools::default();
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Should not run",
            &*provider,
            &tools,
            MAX_ITER,
            None,
            &cancel,
        )
        .await;
        assert!(matches!(result.unwrap_err(), Error::Cancelled));
        assert_eq!(provider.call_count(), 0);
    }

    #[tokio::test]
    async fn test_process_message_saves_on_provider_error() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = crate::workspace::Workspace::init_at(dir.path().to_path_buf()).unwrap();
        let session_path = dir.path().join("sessions").join("test.json");

        let provider = Arc::new(MockProvider::new(vec![Err(ProviderError::Network(
            "connection refused".into(),
        ))]));
        let tools = Tools::default();
        let summarize = test_summarize(&provider);

        let mut engine = FlatSession::new(session_path.clone(), ContextConfig::default()).unwrap();

        let result = process_message(
            &mut engine,
            &summarize,
            &workspace,
            "Hello?",
            &*provider,
            &tools,
            MAX_ITER,
            None,
            &noop_cancel(),
        )
        .await;
        assert!(result.is_err());

        // The caller (actor) is responsible for saving. We verify the engine
        // recorded the user message.
        assert_eq!(engine.stats().message_count, 1);
    }

    // ── Policy violation gate ─────────────────────────────────────────

    fn blocked_call(id: &str) -> ToolCall {
        ToolCall::new(
            id.to_string(),
            ToolFunction {
                name: "mock_blocked".to_string(),
                arguments: "{}".to_string(),
            },
        )
    }

    fn blocked_tool_calls(ids: &[&str]) -> Response {
        Response::ToolCalls {
            content: String::new(),
            calls: ids.iter().map(|&id| blocked_call(id)).collect(),
        }
    }

    fn blocked_tools() -> Tools {
        Tools::new(vec![Box::new(MockBlockedTool::new("not allowed"))], &[]).unwrap()
    }

    #[tokio::test]
    async fn test_first_blocked_injects_system_directive() {
        let provider = Arc::new(MockProvider::new(vec![
            Ok(blocked_tool_calls(&["b1"])),
            Ok(text("OK I'll stop")),
        ]));
        let tools = blocked_tools();
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        let result = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Try blocked",
            &*provider,
            &tools,
            MAX_ITER,
            None,
            &noop_cancel(),
        )
        .await;
        assert_eq!(result.unwrap(), "OK I'll stop");

        let ctx = engine.assemble("").await.unwrap();
        let has_directive = ctx.messages.iter().any(
            |m| matches!(m, Message::System { content } if content.contains("POLICY VIOLATION")),
        );
        assert!(has_directive, "expected POLICY VIOLATION system message");
    }

    #[tokio::test]
    async fn test_second_blocked_halts_turn() {
        let provider = Arc::new(MockProvider::new(vec![
            Ok(blocked_tool_calls(&["b1"])),
            Ok(blocked_tool_calls(&["b2"])),
        ]));
        let tools = blocked_tools();
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        let result = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Keep trying",
            &*provider,
            &tools,
            MAX_ITER,
            None,
            &noop_cancel(),
        )
        .await;

        let msg = result.unwrap();
        assert!(
            msg.contains("halted automatically"),
            "expected halt message, got: {msg}",
        );
        assert!(
            msg.contains("not allowed"),
            "expected blocked reason in halt message, got: {msg}",
        );
    }

    #[tokio::test]
    async fn test_policy_strikes_reset_between_turns() {
        let provider = Arc::new(MockProvider::new(vec![
            Ok(blocked_tool_calls(&["b1"])),
            Ok(text("Turn 1 done")),
            Ok(blocked_tool_calls(&["b2"])),
            Ok(text("Turn 2 done")),
        ]));
        let tools = blocked_tools();
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        let r1 = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Turn 1",
            &*provider,
            &tools,
            MAX_ITER,
            None,
            &noop_cancel(),
        )
        .await;
        assert_eq!(r1.unwrap(), "Turn 1 done");

        let r2 = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Turn 2",
            &*provider,
            &tools,
            MAX_ITER,
            None,
            &noop_cancel(),
        )
        .await;
        assert_eq!(r2.unwrap(), "Turn 2 done");
    }
}
