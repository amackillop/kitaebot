//! Context engine abstraction.
//!
//! All context management flows through the [`ContextEngine`] trait. The agent
//! loop, actor, and channels interact exclusively with this interface.
//!
//! Three implementations exist:
//! - **Flat session** (`flat.rs`): wraps `Session` + `context.rs`. No `SQLite`.
//! - **LCM** (`lcm/`): hierarchical DAG of summaries over `SQLite`.
//! - **Ephemeral** (`ephemeral.rs`): in-memory, for sub-agent turns.

pub mod ephemeral;
pub mod flat;
pub mod lcm;
pub(crate) mod names;
pub(crate) mod stats;

use std::collections::BTreeMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use crate::error::{EngineError, ProviderError};
use crate::provider::Provider;
use crate::tools::Tool;
use crate::types::{Message, Response, ToolDefinition};

/// Callback for LLM summarization during compaction.
///
/// The engine does not own a provider; it borrows summarization
/// capability via this closure. Constructed once at startup via
/// [`make_summarize_fn`], then passed by reference into compaction
/// methods.
///
/// The first argument is the per-call **instruction block**, placed
/// in the user turn alongside the formatted conversation. The system
/// turn is fixed — see `SUMMARIZER_ROLE_PROMPT`. The flat session
/// uses one fixed instruction block; LCM's three-level escalator
/// switches between distinct level-1 and level-2 instruction blocks.
pub type SummarizeFn = Arc<
    dyn Fn(&str, &[Message]) -> Pin<Box<dyn Future<Output = Result<String, ProviderError>> + Send>>
        + Send
        + Sync,
>;

/// Who a tool set is being assembled for.
///
/// Engines contribute different tools depending on the consumer:
/// the root agent gets recall tools only, while sub-agents
/// ([spec 19]) additionally get `lcm_expand` — bulk expansion goes
/// through a child context so it never floods the parent's window.
///
/// [spec 19]: ../specs/19-sub-agents.md
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolScope {
    /// The root agent's registry.
    Root,
    /// A sub-agent's per-type tool set.
    SubAgent,
}

/// Everything the agent loop needs for a provider call.
pub struct AssembledContext {
    /// Ordered messages for the provider (system prompt included).
    pub messages: Vec<Message>,
}

/// Compaction event for activity reporting.
pub struct CompactionEvent {
    /// Estimated tokens before compaction.
    pub before: usize,
    /// Estimated tokens after compaction.
    pub after: usize,
}

/// Context statistics.
pub struct ContextStats {
    /// Estimated token count of current context.
    pub token_estimate: usize,
    /// Token budget (compaction trigger threshold).
    pub budget: usize,
    /// Number of messages in current session.
    pub message_count: usize,
}

/// Metadata about a session.
#[allow(dead_code)] // Used by FlatSession::list_sessions.
pub struct SessionInfo {
    pub name: String,
    pub message_count: usize,
    pub estimated_tokens: usize,
}

/// Context management trait.
///
/// All methods are async (RPIT). The agent loop is generic over this trait,
/// monomorphized at the call site. One engine per agent — generics, not dyn.
pub trait ContextEngine: Send + Sync {
    /// Append a message to the active session.
    fn push_message(
        &mut self,
        msg: Message,
    ) -> impl Future<Output = Result<(), EngineError>> + Send;

    /// Assemble the full context for a provider call.
    fn assemble(
        &self,
        system_prompt: &str,
    ) -> impl Future<Output = Result<AssembledContext, EngineError>> + Send;

    /// Record the provider-reported prompt size of the last request.
    ///
    /// Ground truth for the context size: the provider's tokenizer
    /// counts the system prompt and tool schemas that char-based
    /// estimates miss. Engines take `max(estimate, observed)` when
    /// deciding to compact, and must drop the observation whenever the
    /// context shrinks (compaction, clear, session switch) — it
    /// describes a request that no longer reflects the session.
    fn observe_tokens(&mut self, prompt_tokens: usize);

    /// Record the exact request most recently sent to the provider
    /// for the active session: the assembled messages and the tool
    /// schemas, byte-for-byte. This is the ground truth of what the
    /// provider's implicit prefix cache holds; engines that compact
    /// by riding that cache keep it (spec 14 §"Cache-Prefix Riding"),
    /// others drop it.
    fn observe_request(&mut self, messages: Vec<Message>, tools: Arc<[ToolDefinition]>) {
        let _ = (messages, tools);
    }

    /// Blocking compaction when the context is too large to safely
    /// take another completion. Called before every completion, so it
    /// must be a cheap no-op below its threshold. Compacting here
    /// invalidates the provider's prompt cache mid-turn; engines keep
    /// the threshold high so it fires only as an emergency.
    fn compact_if_urgent(
        &mut self,
        summarize: &SummarizeFn,
    ) -> impl Future<Output = Result<Option<CompactionEvent>, EngineError>> + Send;

    /// Routine compaction between turns, after the reply is delivered.
    /// This is the cache-friendly moment: rewriting history here can
    /// cost at most the next turn's first completion a cache hit,
    /// where the same rewrite mid-turn cold-starts every remaining
    /// completion. Engines without a between-turns policy return
    /// `None`.
    fn compact_between_turns(
        &mut self,
        summarize: &SummarizeFn,
    ) -> impl Future<Output = Result<Option<CompactionEvent>, EngineError>> + Send {
        let _ = summarize;
        async { Ok(None) }
    }

    /// Unconditionally run one compaction cycle.
    fn force_compact(
        &mut self,
        summarize: &SummarizeFn,
    ) -> impl Future<Output = Result<CompactionEvent, EngineError>> + Send;

    /// Clear the active session's history.
    fn clear(&mut self) -> impl Future<Output = Result<(), EngineError>> + Send;

    /// Persist the active session to durable storage.
    fn save(&mut self) -> impl Future<Output = Result<(), EngineError>> + Send;

    /// Current context statistics.
    fn stats(&self) -> ContextStats;

    /// Tools contributed by this engine (empty for flat session).
    ///
    /// Instances are `Arc` so one tool can appear in multiple filtered
    /// sets (root agent, sub-agents) without duplication.
    fn tools(&self, scope: ToolScope) -> Vec<Arc<dyn Tool>>;

    /// Rendered cross-session usage report for `/stats`.
    ///
    /// Engines feed their stored messages through `stats::render`
    /// and may append engine-specific sections (LCM appends a
    /// health section).
    fn report(&self) -> impl Future<Output = Result<String, EngineError>> + Send;

    /// Name of the active session.
    fn active_session(&self) -> &str;

    /// Switch to a named session. Creates it if it does not exist.
    fn switch_session(
        &mut self,
        name: &str,
    ) -> impl Future<Output = Result<(), EngineError>> + Send;

    /// List all available sessions.
    fn list_sessions(&self) -> impl Future<Output = Result<Vec<SessionInfo>, EngineError>> + Send;

    /// Undistilled token totals per session, for the memory
    /// distillation gate (spec 21).
    ///
    /// For each session, sums the stored `token_count` of every event
    /// at or beyond that session's watermark in `since` (a session
    /// missing from the map counts from its first event). Sessions
    /// with no pending events are omitted. Reads counts only, never
    /// content, so the distill duty can probe cheaply before spending an
    /// LLM turn. Ephemeral history is not durable and yields an empty
    /// map.
    fn pending_distill_tokens(
        &self,
        since: &BTreeMap<String, u64>,
    ) -> impl Future<Output = Result<BTreeMap<String, u64>, EngineError>> + Send;

    /// Events for `session` at or beyond position `after`, oldest
    /// first, for a distillation pass (spec 21).
    ///
    /// The span is clamped so its summed `token_count` stays within
    /// `max_tokens`, always returning at least one event when any are
    /// pending so an oversized head cannot stall progress. Positions
    /// are dense, so the caller advances the watermark to
    /// `after + returned.len()`. Ephemeral history is not durable and
    /// yields an empty vec.
    fn transcript_since(
        &self,
        session: &str,
        after: u64,
        max_tokens: u64,
    ) -> impl Future<Output = Result<Vec<Message>, EngineError>> + Send;

    /// Each session's current tip: the position `transcript_since`
    /// would advance to after consuming everything. Used to prime
    /// fresh distillation state so history predating the state
    /// database is grandfathered rather than reprocessed (spec 21) —
    /// the poll cursors' starting-now semantics, in positions.
    /// Sessions with no events are omitted. The default is for engines
    /// with no durable history.
    fn latest_positions(
        &self,
    ) -> impl Future<Output = Result<BTreeMap<String, u64>, EngineError>> + Send {
        async { Ok(BTreeMap::new()) }
    }

    /// Stage the engine's durable state from `context_dir` into
    /// `dest`, for `kitaebot backup` (spec 05). Runs without a
    /// constructed engine; databases must be snapshotted consistently
    /// ([`crate::backup::snapshot_dir`] handles the common layout).
    /// Deliberately no default: a new engine cannot compile without
    /// answering how it is backed up.
    fn backup(context_dir: &Path, dest: &Path) -> Result<(), EngineError>;
}

/// Role-setting system prompt for every summarization call. The
/// caller-supplied instructions go in the user turn alongside the
/// formatted conversation. This split mirrors the reference
/// implementation: keep the system prompt minimal and stable, vary
/// instructions per call in the user message.
const SUMMARIZER_ROLE_PROMPT: &str = "You are a context-compaction \
summarization engine. Follow user instructions exactly and return \
plain text summary content only.";

/// Build a `SummarizeFn` that uses the given provider for LLM calls.
///
/// The provider is captured by `Arc`: one heap allocation, paid once.
/// Each call supplies an instruction block; the closure formats the
/// messages, wraps them in `<conversation_segment>` tags, and combines
/// them with the instructions into a single user turn. The system
/// turn is fixed.
pub fn make_summarize_fn<P: Provider + 'static>(provider: Arc<P>) -> SummarizeFn {
    Arc::new(move |instructions: &str, messages: &[Message]| {
        let provider = provider.clone();
        let user_content = format!(
            "{instructions}\n\n<conversation_segment>\n{}\n</conversation_segment>",
            format_messages_for_summary(messages),
        );
        let prompt_messages = vec![
            Message::System {
                content: SUMMARIZER_ROLE_PROMPT.to_string(),
            },
            Message::User {
                content: user_content,
            },
        ];

        Box::pin(async move {
            let outcome = provider.chat("summarizer", &prompt_messages, &[]).await?;
            match outcome.response {
                Response::Text(text) => Ok(text),
                Response::ToolCalls { content, .. } => Ok(content),
            }
        })
    })
}

/// Callback for one raw provider call during cache-prefix riding
/// (spec 14 §"Cache-Prefix Riding"): session key, full message
/// vector, tool schemas — the caller controls every byte of the
/// request, unlike [`SummarizeFn`], which owns the prompt shape.
///
/// Always built over the **main** provider: the prompt cache being
/// ridden lives under the main model, so a summarizer model override
/// must not apply here.
pub type RawChatFn = Arc<
    dyn Fn(
            String,
            Vec<Message>,
            Arc<[ToolDefinition]>,
        ) -> Pin<Box<dyn Future<Output = Result<String, ProviderError>> + Send>>
        + Send
        + Sync,
>;

/// Build a `RawChatFn` over the given provider.
pub fn make_raw_chat_fn<P: Provider + 'static>(provider: Arc<P>) -> RawChatFn {
    Arc::new(
        move |session: String, messages: Vec<Message>, tools: Arc<[ToolDefinition]>| {
            let provider = provider.clone();
            Box::pin(async move {
                let outcome = provider.chat(&session, &messages, &tools).await?;
                match outcome.response {
                    Response::Text(text) => Ok(text),
                    Response::ToolCalls { content, .. } => Ok(content),
                }
            })
        },
    )
}

/// Tail-biased truncation for tool result content.
///
/// Engines that cannot externalize to disk (flat, ephemeral) cap tool
/// output at `max_tokens` estimated tokens by keeping half from the
/// head and half from the tail, with a marker in between. The tail is
/// kept deliberately: build and test logs put the failure at the end,
/// which head-only truncation used to destroy.
pub(crate) fn truncate_tool_output(content: &str, max_tokens: usize) -> std::borrow::Cow<'_, str> {
    if crate::types::estimate_tokens(content) <= max_tokens {
        return std::borrow::Cow::Borrowed(content);
    }
    // Bytes kept per side; over-threshold content is strictly longer
    // than both sides combined, so the slices never overlap.
    let keep = max_tokens * 4 / 2;
    let mut head_end = keep;
    while !content.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = content.len() - keep;
    while !content.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let omitted = crate::types::estimate_tokens(&content[head_end..tail_start]);
    std::borrow::Cow::Owned(format!(
        "{}\n... [~{omitted} tokens truncated] ...\n{}",
        &content[..head_end],
        &content[tail_start..],
    ))
}

pub(crate) fn format_messages_for_summary(messages: &[Message]) -> String {
    let mut out = String::new();
    for msg in messages {
        match msg {
            Message::Assistant { content } => {
                out.push_str("[assistant] ");
                out.push_str(content);
            }
            Message::System { content } => {
                out.push_str("[system] ");
                out.push_str(content);
            }
            Message::Tool { call_id, content } => {
                out.push_str("[tool:");
                out.push_str(call_id);
                out.push_str("] ");
                out.push_str(content);
            }
            Message::ToolCalls { content, calls } => {
                out.push_str("[assistant] ");
                out.push_str(content);
                for tc in calls {
                    out.push_str("\n  [tool_call] ");
                    out.push_str(tc.function.name.as_str());
                    out.push('(');
                    out.push_str(&tc.function.arguments);
                    out.push(')');
                }
            }
            Message::User { content } => {
                out.push_str("[user] ");
                out.push_str(content);
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::MockProvider;
    use crate::types::Response;

    #[test]
    fn truncate_tool_output_passes_small_content_through() {
        let content = "short output";
        let result = truncate_tool_output(content, 100);
        assert!(matches!(result, std::borrow::Cow::Borrowed(_)));
        assert_eq!(result, content);
    }

    #[test]
    fn truncate_tool_output_keeps_head_and_tail() {
        let content = format!("HEAD{}TAIL", "x".repeat(10_000));
        let result = truncate_tool_output(&content, 100);
        assert!(result.starts_with("HEAD"));
        assert!(result.ends_with("TAIL"));
        assert!(result.contains("tokens truncated] ..."));
        // 100 tokens = 400 bytes kept plus the marker.
        assert!(result.len() < 500);
    }

    #[test]
    fn truncate_tool_output_is_multibyte_safe() {
        let content = "€".repeat(10_000);
        let result = truncate_tool_output(&content, 100);
        assert!(result.len() < content.len());
        assert!(result.contains("tokens truncated"));
    }

    #[tokio::test]
    async fn summarize_fn_calls_provider() {
        let provider = Arc::new(MockProvider::new(vec![Ok(Response::Text(
            "summary".to_string(),
        ))]));
        let summarize = make_summarize_fn(provider.clone());

        let messages = vec![
            Message::User {
                content: "hello".to_string(),
            },
            Message::Assistant {
                content: "hi".to_string(),
            },
        ];

        let result = summarize("test prompt", &messages).await.unwrap();
        assert_eq!(result, "summary");
        assert_eq!(provider.call_count(), 1);
    }

    #[tokio::test]
    async fn summarize_fn_handles_tool_calls_response() {
        let provider = Arc::new(MockProvider::new(vec![Ok(Response::ToolCalls {
            content: "fallback text".to_string(),
            calls: vec![],
        })]));
        let summarize = make_summarize_fn(provider);

        let result = summarize("p", &[]).await.unwrap();
        assert_eq!(result, "fallback text");
    }

    #[tokio::test]
    async fn summarize_fn_propagates_error() {
        let provider = Arc::new(MockProvider::new(vec![Err(ProviderError::RateLimited)]));
        let summarize = make_summarize_fn(provider);

        let result = summarize("p", &[]).await;
        assert!(matches!(result, Err(ProviderError::RateLimited)));
    }

    #[test]
    fn format_messages_covers_all_variants() {
        let messages = vec![
            Message::System {
                content: "sys".to_string(),
            },
            Message::User {
                content: "usr".to_string(),
            },
            Message::Assistant {
                content: "ast".to_string(),
            },
            Message::ToolCalls {
                content: "thinking".to_string(),
                calls: vec![crate::types::ToolCall::new(
                    "c1".to_string(),
                    crate::types::ToolFunction {
                        name: "exec".parse().unwrap(),
                        arguments: r#"{"cmd":"ls"}"#.to_string(),
                    },
                )],
            },
            Message::Tool {
                call_id: "c1".to_string(),
                content: "file.txt".to_string(),
            },
        ];

        let formatted = format_messages_for_summary(&messages);
        assert!(formatted.contains("[system] sys"));
        assert!(formatted.contains("[user] usr"));
        assert!(formatted.contains("[assistant] ast"));
        assert!(formatted.contains("[assistant] thinking"));
        assert!(formatted.contains("[tool_call] exec"));
        assert!(formatted.contains("[tool:c1] file.txt"));
    }
}
