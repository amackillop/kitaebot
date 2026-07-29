//! Push notifications to the user (spec 17).
//!
//! The `notify` tool lets the agent reach the user outside the current
//! request-reply flow, via Telegram. `high` urgency sends immediately;
//! `low` urgency is batched and flushed by the actor after the turn.
//!
//! The tool is registered once at startup but the batch buffer and rate
//! counter are per-turn, so both live in a [`Notifier`] shared as
//! `Arc` between the tool and the actor. State transitions are pure
//! ([`NotifyState`]); the lock is never held across an await.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use schemars::JsonSchema;
use serde::Deserialize;
use tracing::warn;

use crate::clients::telegram::TelegramClient;
use crate::error::{TelegramError, ToolError};
use crate::tools::{Tool, ToolCtx, truncate_output};

/// Max notify calls per turn, both urgencies counted. Failed sends
/// still consume a slot so the agent can't hammer the Telegram API.
const MAX_PER_TURN: u8 = 5;

/// Byte cap for outgoing text — under Telegram's 4096-character
/// message limit with room for the truncation marker.
const MAX_MESSAGE_BYTES: usize = 4000;

#[derive(Deserialize, JsonSchema, Default, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Urgency {
    /// Batched: delivered as one message after the turn completes.
    #[default]
    Low,
    /// Immediate: sent as soon as the tool executes.
    High,
}

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Content to send to the user.
    message: String,
    /// "low" (default): batched, delivered after the turn completes.
    /// "high": sent immediately.
    #[serde(default)]
    urgency: Urgency,
}

/// Per-turn notification state. Pure — no IO.
#[derive(Default)]
struct NotifyState {
    attempts: u8,
    batch: Vec<String>,
}

/// Outcome of recording a notify call against the per-turn state.
enum NotifyAction {
    Buffered,
    RateLimited,
    SendNow(String),
}

impl NotifyState {
    fn record(&mut self, message: String, urgency: Urgency) -> NotifyAction {
        if self.attempts >= MAX_PER_TURN {
            return NotifyAction::RateLimited;
        }
        self.attempts += 1;
        match urgency {
            Urgency::Low => {
                self.batch.push(message);
                NotifyAction::Buffered
            }
            Urgency::High => NotifyAction::SendNow(message),
        }
    }

    /// Take the batch, joined with blank lines. `None` when empty.
    fn drain(&mut self) -> Option<String> {
        if self.batch.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.batch).join("\n\n"))
        }
    }
}

/// Shared notification sink: owns the Telegram client and the
/// per-turn state. The tool records calls and performs immediate
/// sends; the actor resets the state before each turn and flushes
/// the batch after.
pub struct Notifier {
    client: TelegramClient,
    chat_id: i64,
    state: Mutex<NotifyState>,
    /// Durable mirror of every outgoing message (`None` in tests).
    log_path: Option<std::path::PathBuf>,
}

impl Notifier {
    pub fn new(client: TelegramClient, chat_id: i64) -> Self {
        Self {
            client,
            chat_id,
            state: Mutex::new(NotifyState::default()),
            log_path: None,
        }
    }

    /// Mirror every outgoing message to an append-only log, so
    /// notifications are greppable without Telegram.
    pub fn with_log(mut self, path: std::path::PathBuf) -> Self {
        self.log_path = Some(path);
        self
    }

    /// Reset the rate counter and batch. Called by the actor before
    /// each turn.
    pub fn begin_turn(&self) {
        *self.state.lock().unwrap() = NotifyState::default();
    }

    /// Deliver the batched low-urgency messages as one Telegram
    /// message. Best-effort: the turn is already over, so a failure
    /// is logged and dropped.
    pub async fn flush(&self) {
        let Some(text) = self.state.lock().unwrap().drain() else {
            return;
        };
        if let Err(e) = self.send(&text).await {
            warn!("Failed to deliver batched notifications: {e}");
        }
    }

    /// Harness-initiated immediate send. Bypasses the per-turn rate
    /// counter: only the actor calls this, at most once per turn.
    /// Best-effort, like `flush`.
    pub async fn alert(&self, text: &str) {
        if let Err(e) = self.send(text).await {
            warn!("Failed to deliver alert: {e}");
        }
    }

    fn record(&self, message: String, urgency: Urgency) -> NotifyAction {
        self.state.lock().unwrap().record(message, urgency)
    }

    /// Plain-text, single-attempt send, truncated to fit Telegram's
    /// message cap. Deliberately bypasses the Telegram channel's
    /// retry/escape layer: notification delivery is best-effort.
    async fn send(&self, text: &str) -> Result<(), TelegramError> {
        let text = truncate_output(text, MAX_MESSAGE_BYTES);
        // The mirror, not Telegram, is the durable record: log before
        // the send so a failed delivery still leaves one.
        if let Some(path) = &self.log_path
            && let Err(e) = crate::workspace::append_log(path, &text)
        {
            warn!("failed to mirror notification: {e}");
        }
        self.client
            .post_message(self.chat_id, &text, None)
            .await
            .map(|_| ())
    }
}

/// Tool that pushes a notification to the user's phone.
pub struct NotifyTool(pub Arc<Notifier>);

impl Tool for NotifyTool {
    fn name(&self) -> &'static str {
        "notify"
    }

    fn description(&self) -> &'static str {
        "Push a notification to the user's phone via Telegram. Use this to \
        proactively surface something worth the user's attention — a finished \
        long-running task, a blocked work item, an important finding. Keep the \
        substance in your normal reply or the relevant channel; the \
        notification is the attention tap.\n\n\
        urgency \"low\" (default): batched with other low-urgency \
        notifications and delivered as one message after this turn.\n\
        urgency \"high\": delivered immediately.\n\n\
        Limited to 5 calls per turn."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(Args)).expect("schema serialization failed")
    }

    fn execute(
        &self,
        args: serde_json::Value,
        _ctx: ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let args: Args = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

            match self.0.record(args.message, args.urgency) {
                NotifyAction::Buffered => {
                    Ok("Notification queued for delivery after this turn.".into())
                }
                NotifyAction::RateLimited => Err(ToolError::ExecutionFailed(format!(
                    "notification rate limit reached ({MAX_PER_TURN} per turn)"
                ))),
                NotifyAction::SendNow(message) => {
                    self.0
                        .send(&message)
                        .await
                        .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
                    Ok("Notification sent.".into())
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;
    use crate::clients::RawResponse;

    // -- Pure core --

    #[test]
    fn low_urgency_buffers() {
        let mut state = NotifyState::default();
        let action = state.record("hello".into(), Urgency::Low);
        assert!(matches!(action, NotifyAction::Buffered));
        assert_eq!(state.batch, vec!["hello".to_string()]);
    }

    #[test]
    fn high_urgency_sends_now() {
        let mut state = NotifyState::default();
        let action = state.record("urgent".into(), Urgency::High);
        let NotifyAction::SendNow(msg) = action else {
            panic!("expected SendNow");
        };
        assert_eq!(msg, "urgent");
        assert!(state.batch.is_empty());
    }

    #[test]
    fn sixth_call_rate_limited() {
        let mut state = NotifyState::default();
        for i in 0..5 {
            let urgency = if i % 2 == 0 {
                Urgency::Low
            } else {
                Urgency::High
            };
            assert!(!matches!(
                state.record(format!("msg {i}"), urgency),
                NotifyAction::RateLimited
            ));
        }
        assert!(matches!(
            state.record("one too many".into(), Urgency::High),
            NotifyAction::RateLimited
        ));
    }

    #[test]
    fn drain_joins_with_blank_line() {
        let mut state = NotifyState::default();
        state.record("first".into(), Urgency::Low);
        state.record("second".into(), Urgency::Low);
        assert_eq!(state.drain().unwrap(), "first\n\nsecond");
        assert!(state.drain().is_none());
    }

    #[test]
    fn drain_empty_returns_none() {
        assert!(NotifyState::default().drain().is_none());
    }

    #[test]
    fn begin_turn_resets_count_and_batch() {
        let notifier = Notifier::new(ok_client(&sent()), 42);
        for _ in 0..5 {
            notifier.record("x".into(), Urgency::Low);
        }
        assert!(matches!(
            notifier.record("y".into(), Urgency::Low),
            NotifyAction::RateLimited
        ));
        notifier.begin_turn();
        assert!(matches!(
            notifier.record("z".into(), Urgency::Low),
            NotifyAction::Buffered
        ));
        assert_eq!(notifier.state.lock().unwrap().batch, vec!["z".to_string()]);
    }

    // -- Tool via fake client --

    type Sent = Arc<Mutex<Vec<(String, serde_json::Value)>>>;

    fn sent() -> Sent {
        Arc::new(Mutex::new(Vec::new()))
    }

    /// Client that records every call and answers with a successful
    /// sendMessage response.
    fn ok_client(sent: &Sent) -> TelegramClient {
        let sent = sent.clone();
        TelegramClient::from_fn(move |method, body| {
            let sent = sent.clone();
            async move {
                sent.lock()
                    .unwrap()
                    .push((method, serde_json::from_slice(&body).unwrap()));
                Ok(RawResponse {
                    status: 200,
                    body: br#"{"ok":true,"result":{"message_id":1,"chat":{"id":42},"text":null}}"#
                        .to_vec(),
                })
            }
        })
    }

    fn tool(sent: &Sent) -> NotifyTool {
        NotifyTool(Arc::new(Notifier::new(ok_client(sent), 42)))
    }

    #[tokio::test]
    async fn high_urgency_posts_immediately() {
        let sent = sent();
        let tool = tool(&sent);

        let result = tool
            .execute(
                json!({"message": "build is green", "urgency": "high"}),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert_eq!(result, "Notification sent.");

        let calls = sent.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let (method, body) = &calls[0];
        assert_eq!(method, "sendMessage");
        assert_eq!(body["chat_id"], 42);
        assert_eq!(body["text"], "build is green");
        assert!(body.get("parse_mode").is_none());
    }

    #[tokio::test]
    async fn low_urgency_posts_nothing_until_flush() {
        let sent = sent();
        let tool = tool(&sent);

        for msg in ["first", "second"] {
            tool.execute(json!({"message": msg}), ToolCtx::default())
                .await
                .unwrap();
        }
        assert!(sent.lock().unwrap().is_empty());

        tool.0.flush().await;

        let calls = sent.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1["text"], "first\n\nsecond");
    }

    #[tokio::test]
    async fn flush_with_empty_batch_posts_nothing() {
        let sent = sent();
        let tool = tool(&sent);
        tool.0.flush().await;
        assert!(sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn sends_are_mirrored_to_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTIFICATIONS.md");
        let sent = sent();
        let tool = NotifyTool(Arc::new(
            Notifier::new(ok_client(&sent), 42).with_log(path.clone()),
        ));

        tool.execute(
            json!({"message": "build is green", "urgency": "high"}),
            ToolCtx::default(),
        )
        .await
        .unwrap();

        let log = std::fs::read_to_string(&path).unwrap();
        assert!(log.contains("build is green"));
    }

    #[tokio::test]
    async fn failed_delivery_still_leaves_a_log_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTIFICATIONS.md");
        let client = TelegramClient::from_fn(|_method, _body| async {
            Err(TelegramError::Network("boom".into()))
        });
        let notifier = Notifier::new(client, 42).with_log(path.clone());

        notifier.alert("the daemon is on fire").await;

        let log = std::fs::read_to_string(&path).unwrap();
        assert!(
            log.contains("the daemon is on fire"),
            "the mirror is the record; delivery failure must not erase it"
        );
    }

    #[tokio::test]
    async fn telegram_failure_surfaces_execution_failed() {
        let client = TelegramClient::from_fn(|_method, _body| async {
            Err(TelegramError::Network("boom".into()))
        });
        let tool = NotifyTool(Arc::new(Notifier::new(client, 42)));

        let err = tool
            .execute(
                json!({"message": "x", "urgency": "high"}),
                ToolCtx::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed(_)));
    }

    #[tokio::test]
    async fn rate_limit_returns_error_to_agent() {
        let sent = sent();
        let tool = tool(&sent);

        for _ in 0..5 {
            tool.execute(
                json!({"message": "x", "urgency": "high"}),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        }
        let err = tool
            .execute(
                json!({"message": "x", "urgency": "high"}),
                ToolCtx::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed(_)));
        assert_eq!(sent.lock().unwrap().len(), 5);
    }

    #[tokio::test]
    async fn invalid_urgency_rejected() {
        let sent = sent();
        let tool = tool(&sent);

        let err = tool
            .execute(
                json!({"message": "x", "urgency": "shouting"}),
                ToolCtx::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
        assert!(sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn oversize_message_truncated() {
        let sent = sent();
        let tool = tool(&sent);

        let long = "a".repeat(MAX_MESSAGE_BYTES + 500);
        tool.execute(
            json!({"message": long, "urgency": "high"}),
            ToolCtx::default(),
        )
        .await
        .unwrap();

        let calls = sent.lock().unwrap();
        let text = calls[0].1["text"].as_str().unwrap();
        assert!(text.len() < MAX_MESSAGE_BYTES + 100);
        assert!(text.contains("[truncated"));
    }
}
