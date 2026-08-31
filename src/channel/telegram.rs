//! Telegram Bot API channel.
//!
//! Long-polls `getUpdates` for incoming messages, sends them to the
//! agent actor via [`AgentHandle`], and sends responses back via
//! `sendMessage`. Designed to run as an async loop alongside the
//! duty scheduler in the daemon's `tokio::select!`.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::agent::AgentHandle;
use crate::agent::envelope::ChannelSource;
use crate::clients::telegram::TelegramClient;
use crate::error::TelegramError;

// --- Channel ---

/// Maximum retries for `sendMessage` on transient failures.
const SEND_RETRIES: u32 = 3;

/// Cap for poll-loop exponential backoff (1s, 2s, 4s, ..., 60s).
const MAX_POLL_BACKOFF_SECS: u64 = 60;

/// Telegram Bot API channel.
///
/// Wraps a [`TelegramClient`] and the chat routing configuration.
/// The client handles raw HTTP; this struct layers retry logic,
/// HTML escaping, and message formatting on top.
pub struct TelegramChannel {
    client: TelegramClient,
    chat_id: i64,
    poll_timeout_secs: u64,
}

impl TelegramChannel {
    pub fn new(client: TelegramClient, chat_id: i64, poll_timeout_secs: u64) -> Self {
        Self {
            client,
            chat_id,
            poll_timeout_secs,
        }
    }

    pub fn chat_id(&self) -> i64 {
        self.chat_id
    }

    /// Send a plain text message with retries on transient failures.
    ///
    /// Retries up to [`SEND_RETRIES`] times with exponential backoff
    /// (1s, 2s, 4s) on network errors and 429/5xx API responses.
    async fn send_message(&self, text: &str) -> Result<(), TelegramError> {
        self.send_raw(text, None).await
    }

    /// Send preformatted text rendered in a monospace font.
    async fn send_preformatted(&self, text: &str) -> Result<(), TelegramError> {
        let escaped = html_escape(text);
        let html = format!("<pre>{escaped}</pre>");
        self.send_raw(&html, Some("HTML")).await
    }

    async fn send_raw(&self, text: &str, parse_mode: Option<&str>) -> Result<(), TelegramError> {
        let mut attempts = 0u32;
        loop {
            if let Err(e) = self
                .client
                .post_message(self.chat_id, text, parse_mode)
                .await
            {
                if attempts < SEND_RETRIES && is_transient(&e) {
                    let delay = Duration::from_secs(u64::from(1u32 << attempts));
                    attempts += 1;
                    warn!(
                        attempt = attempts,
                        "send_message retrying in {delay:?}: {e}"
                    );
                    tokio::time::sleep(delay).await;
                } else {
                    return Err(e);
                }
            } else {
                return Ok(());
            }
        }
    }
}

/// Escape the three characters that are special in Telegram HTML mode.
fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Whether a [`TelegramError`] is worth retrying.
fn is_transient(err: &TelegramError) -> bool {
    match err {
        TelegramError::Network(_) => true,
        TelegramError::Api { error_code, .. } => *error_code >= 500 || *error_code == 429,
        TelegramError::Deserialize(_) => false,
        #[cfg(test)]
        TelegramError::Session(_) => false,
    }
}

/// Backoff delay for a transient poll-loop error.
///
/// Respects Telegram's `retry_after` when present (429 responses).
/// Otherwise uses exponential backoff from `attempts`: 1s, 2s, 4s,
/// capped at 60s.
fn poll_backoff(err: &TelegramError, attempts: u32) -> Duration {
    if let Some(secs) = err.retry_after() {
        return Duration::from_secs(secs);
    }
    let secs = u64::from(1u32 << attempts.min(6));
    Duration::from_secs(MAX_POLL_BACKOFF_SECS.min(secs))
}

// --- Poll loop ---

/// Run the Telegram long-poll loop until cancelled.
///
/// This future never resolves normally — it loops forever, yielding at
/// each `getUpdates` call. The parent `tokio::select!` drops it on
/// shutdown.
pub async fn poll_loop(channel: &TelegramChannel, handle: &AgentHandle) -> ! {
    info!(chat_id = channel.chat_id(), "Telegram poller starting");
    let mut offset: i64 = 0;
    let mut verbose = false;
    let mut backoff_attempts: u32 = 0;

    loop {
        let updates = match channel
            .client
            .poll_updates(offset, channel.poll_timeout_secs)
            .await
        {
            Ok(updates) => updates,
            Err(TelegramError::Network(ref e)) => {
                error!("Telegram poll error: {e}");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
            Err(e) if is_transient(&e) => {
                let delay = poll_backoff(&e, backoff_attempts);
                backoff_attempts = backoff_attempts.saturating_add(1);
                error!("Telegram API error: {e}; retrying in {delay:?}");
                tokio::time::sleep(delay).await;
                continue;
            }
            Err(e) => {
                error!("Telegram API error: {e}");
                continue;
            }
        };

        backoff_attempts = 0;

        for update in updates {
            offset = update.update_id + 1;

            let Some(message) = update.message else {
                debug!(update_id = update.update_id, "Non-message update, skipping");
                continue;
            };

            if message.chat.id != channel.chat_id() {
                warn!(
                    chat_id = message.chat.id,
                    expected = channel.chat_id(),
                    "Message from unauthorized chat, skipping",
                );
                continue;
            }

            let Some(text) = message.text else {
                debug!("Non-text message, skipping");
                continue;
            };

            handle_message(channel, handle, &text, &mut verbose).await;
        }
    }
}

/// Process a single authorized text message.
async fn handle_message(
    channel: &TelegramChannel,
    handle: &AgentHandle,
    text: &str,
    verbose: &mut bool,
) {
    let trimmed = text.trim();

    // /verbose is UI state, not a slash command — intercept before dispatch.
    if trimmed == "/verbose" {
        *verbose = !*verbose;
        let label = if *verbose { "on" } else { "off" };
        if let Err(e) = channel.send_message(&format!("Verbose: {label}")).await {
            error!("Failed to send verbose toggle: {e}");
        }
        return;
    }

    let (tx, mut rx) = mpsc::channel(64);
    let cancel = CancellationToken::new();

    let result = {
        let reply_fut = handle.send_message(
            ChannelSource::Telegram,
            trimmed.to_string(),
            None,
            Some(tx),
            cancel,
        );
        tokio::pin!(reply_fut);

        loop {
            tokio::select! {
                biased;
                Some(event) = rx.recv() => {
                    if *verbose
                        && let Err(e) = channel.send_message(&event.to_string()).await
                    {
                        error!("Failed to send activity: {e}");
                    }
                }
                result = &mut reply_fut => break result,
            }
        }
    };

    // Drain remaining buffered events.
    while let Ok(event) = rx.try_recv() {
        if *verbose && let Err(e) = channel.send_message(&event.to_string()).await {
            error!("Failed to send activity: {e}");
        }
    }

    let send_result = match result {
        Ok(ref reply) if reply.preformatted => channel.send_preformatted(&reply.content).await,
        Ok(ref reply) => channel.send_message(&reply.content).await,
        Err(ref msg) => channel.send_message(msg).await,
    };
    if let Err(e) = send_result {
        error!("Failed to send response: {e}");
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::clients::RawResponse;
    use crate::clients::telegram::{
        ApiResponse, Chat, ResponseParameters, TelegramClient, TgMessage, Update,
    };
    use crate::provider::MockProvider;
    use crate::test_support::{TestAgent, workspace};
    use crate::types::Response as AgentResponse;

    // -- Fake Telegram state for channel tests --

    /// A captured `sendMessage` call.
    #[derive(Clone, Debug)]
    struct SentMessage {
        chat_id: i64,
        text: String,
        parse_mode: Option<String>,
    }

    /// Fake Telegram state that captures outgoing messages and returns
    /// pre-configured results.
    ///
    /// Used via `TelegramClient::from_fn()` closing over an `Arc<Self>`.
    ///
    /// `getUpdates` pops from the poll queue; when empty it returns
    /// a permanently pending future so `tokio::select!` can cancel
    /// the loop via a timeout.
    struct FakeTelegram {
        poll_results: Mutex<VecDeque<Vec<Update>>>,
        send_results: Vec<Result<(), TelegramError>>,
        send_index: AtomicUsize,
        sent: Mutex<Vec<SentMessage>>,
        last_poll_timeout: Mutex<Option<u64>>,
    }

    impl FakeTelegram {
        fn new(send_results: Vec<Result<(), TelegramError>>) -> Arc<Self> {
            Arc::new(Self {
                poll_results: Mutex::new(VecDeque::new()),
                send_results,
                send_index: AtomicUsize::new(0),
                sent: Mutex::new(Vec::new()),
                last_poll_timeout: Mutex::new(None),
            })
        }

        fn with_poll_results(self: Arc<Self>, results: Vec<Vec<Update>>) -> Arc<Self> {
            *self
                .poll_results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = results.into();
            self
        }

        fn sent_messages(&self) -> Vec<SentMessage> {
            self.sent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        fn last_poll_timeout(&self) -> Option<u64> {
            *self
                .last_poll_timeout
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }

        fn ok_json(result: &impl serde::Serialize) -> Vec<u8> {
            serde_json::to_vec(&ApiResponse {
                ok: true,
                result: Some(result),
                error_code: None,
                description: None,
                parameters: None,
            })
            .unwrap()
        }

        fn error_json(error_code: i32, description: &str, retry_after: Option<u64>) -> Vec<u8> {
            serde_json::to_vec(&ApiResponse::<serde_json::Value> {
                ok: false,
                result: None,
                error_code: Some(error_code),
                description: Some(description.into()),
                parameters: retry_after.map(|secs| ResponseParameters {
                    retry_after: Some(secs),
                }),
            })
            .unwrap()
        }
    }

    fn fake_client(state: &Arc<FakeTelegram>) -> TelegramClient {
        let state = Arc::clone(state);
        TelegramClient::from_fn(move |method, body| {
            let state = Arc::clone(&state);
            async move {
                match method.as_str() {
                    "getUpdates" => {
                        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                        *state
                            .last_poll_timeout
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            body["timeout"].as_u64();
                        let next = state
                            .poll_results
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .pop_front();
                        match next {
                            Some(updates) => Ok(RawResponse {
                                status: 200,
                                body: FakeTelegram::ok_json(&updates),
                                retry_after_secs: None,
                            }),
                            None => std::future::pending().await,
                        }
                    }
                    "sendMessage" => {
                        let msg: serde_json::Value = serde_json::from_slice(&body).unwrap();
                        let chat_id = msg["chat_id"].as_i64().unwrap();
                        let text = msg["text"].as_str().unwrap().to_string();
                        let parse_mode = msg["parse_mode"].as_str().map(str::to_string);
                        let index = state.send_index.fetch_add(1, Ordering::SeqCst);
                        state
                            .sent
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(SentMessage {
                                chat_id,
                                text: text.clone(),
                                parse_mode,
                            });

                        match &state.send_results[index] {
                            Ok(()) => Ok(RawResponse {
                                status: 200,
                                body: FakeTelegram::ok_json(&TgMessage {
                                    message_id: 1,
                                    chat: Chat { id: chat_id },
                                    text: Some(text),
                                }),
                                retry_after_secs: None,
                            }),
                            Err(TelegramError::Api {
                                error_code,
                                description,
                                retry_after,
                            }) => Ok(RawResponse {
                                status: 200,
                                body: FakeTelegram::error_json(
                                    *error_code,
                                    description,
                                    *retry_after,
                                ),
                                retry_after_secs: None,
                            }),
                            Err(TelegramError::Network(msg)) => {
                                Err(TelegramError::Network(msg.clone()))
                            }
                            Err(TelegramError::Deserialize(msg)) => {
                                panic!("Unexpected Deserialize error in test stub: {msg}")
                            }
                            Err(TelegramError::Session(msg)) => Ok(RawResponse {
                                status: 200,
                                body: FakeTelegram::error_json(0, msg, None),
                                retry_after_secs: None,
                            }),
                        }
                    }
                    other => panic!("Unexpected Telegram method: {other}"),
                }
            }
        })
    }

    // -- Helpers --

    const CHAT_ID: i64 = 42;

    fn channel(state: &Arc<FakeTelegram>) -> TelegramChannel {
        TelegramChannel::new(fake_client(state), CHAT_ID, 30)
    }

    fn spawn_handle(
        responses: Vec<Result<AgentResponse, crate::error::ProviderError>>,
    ) -> (AgentHandle, tempfile::TempDir) {
        let (dir, ws) = workspace();
        let provider = Arc::new(MockProvider::new(responses));
        let handle = TestAgent::new(ws, provider).spawn();
        (handle, dir)
    }

    // -- Pure unit tests --

    #[test]
    fn transient_error_classification() {
        assert!(is_transient(&TelegramError::Api {
            error_code: 500,
            description: "Internal Server Error".into(),
            retry_after: None,
        }));
        assert!(is_transient(&TelegramError::Api {
            error_code: 429,
            description: "Too Many Requests".into(),
            retry_after: None,
        }));
        assert!(!is_transient(&TelegramError::Api {
            error_code: 400,
            description: "Bad Request".into(),
            retry_after: None,
        }));
        assert!(!is_transient(&TelegramError::Session("test".into())));
    }

    #[test]
    fn poll_backoff_respects_retry_after() {
        let err = TelegramError::Api {
            error_code: 429,
            description: "Too Many Requests".into(),
            retry_after: Some(10),
        };
        assert_eq!(poll_backoff(&err, 0), Duration::from_secs(10));
    }

    #[test]
    fn poll_backoff_exponential_without_retry_after() {
        let err = TelegramError::Api {
            error_code: 500,
            description: "Internal Server Error".into(),
            retry_after: None,
        };
        assert_eq!(poll_backoff(&err, 0), Duration::from_secs(1));
        assert_eq!(poll_backoff(&err, 1), Duration::from_secs(2));
        assert_eq!(poll_backoff(&err, 2), Duration::from_secs(4));
        assert_eq!(poll_backoff(&err, 6), Duration::from_mins(1));
    }

    #[test]
    fn html_escape_special_chars() {
        assert_eq!(html_escape("a < b & c > d"), "a &lt; b &amp; c &gt; d");
    }

    #[test]
    fn html_escape_passthrough() {
        assert_eq!(html_escape("hello world"), "hello world");
    }

    // -- TelegramChannel send tests --

    #[tokio::test]
    async fn send_message_succeeds_on_first_try() {
        let state = FakeTelegram::new(vec![Ok(())]);
        let ch = channel(&state);

        ch.send_message("hello").await.unwrap();

        let sent = state.sent_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].chat_id, CHAT_ID);
        assert_eq!(sent[0].text, "hello");
        assert!(sent[0].parse_mode.is_none());
    }

    #[tokio::test]
    async fn send_raw_retries_on_transient_then_succeeds() {
        tokio::time::pause();

        let state = FakeTelegram::new(vec![
            Err(TelegramError::Network("timeout".into())),
            Err(TelegramError::Api {
                error_code: 429,
                description: "Too Many Requests".into(),
                retry_after: None,
            }),
            Ok(()),
        ]);
        let ch = channel(&state);

        ch.send_message("retry me").await.unwrap();

        assert_eq!(state.sent_messages().len(), 3);
    }

    #[tokio::test]
    async fn send_raw_does_not_retry_non_transient_error() {
        let state = FakeTelegram::new(vec![Err(TelegramError::Api {
            error_code: 400,
            description: "Bad Request".into(),
            retry_after: None,
        })]);
        let ch = channel(&state);

        let err = ch.send_message("bad").await.unwrap_err();

        assert_eq!(state.sent_messages().len(), 1);
        assert!(matches!(
            err,
            TelegramError::Api {
                error_code: 400,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn send_raw_exhausts_retries() {
        tokio::time::pause();

        // SEND_RETRIES (3) transient failures, then one more — the 4th
        // attempt never happens because we've exhausted all retries.
        let state = FakeTelegram::new(vec![
            Err(TelegramError::Network("1".into())),
            Err(TelegramError::Network("2".into())),
            Err(TelegramError::Network("3".into())),
            Err(TelegramError::Network("4".into())),
        ]);
        let ch = channel(&state);

        let err = ch.send_message("doomed").await.unwrap_err();

        // 1 initial + 3 retries = 4 attempts total
        assert_eq!(state.sent_messages().len(), 4);
        assert!(matches!(err, TelegramError::Network(_)));
    }

    #[tokio::test]
    async fn send_preformatted_escapes_html() {
        let state = FakeTelegram::new(vec![Ok(())]);
        let ch = channel(&state);

        ch.send_preformatted("a < b & c > d").await.unwrap();

        let sent = state.sent_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].text, "<pre>a &lt; b &amp; c &gt; d</pre>");
        assert_eq!(sent[0].parse_mode.as_deref(), Some("HTML"));
    }

    // -- handle_message tests --

    #[tokio::test]
    async fn handle_message_dispatches_and_sends_reply() {
        let state = FakeTelegram::new(vec![Ok(())]);
        let ch = channel(&state);
        let (handle, _dir) = spawn_handle(vec![Ok(AgentResponse::Text("pong".into()))]);
        let mut verbose = false;

        handle_message(&ch, &handle, "ping", &mut verbose).await;

        let sent = state.sent_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].text, "pong");
        assert!(sent[0].parse_mode.is_none());
    }

    #[tokio::test]
    async fn handle_message_sends_preformatted_for_preformatted_reply() {
        let state = FakeTelegram::new(vec![Ok(())]);
        let ch = channel(&state);
        let (handle, _dir) = spawn_handle(vec![]);
        let mut verbose = false;

        handle_message(&ch, &handle, "/stats", &mut verbose).await;

        let sent = state.sent_messages();
        assert_eq!(sent.len(), 1);
        // /stats returns preformatted, so it's HTML-escaped and wrapped in <pre>
        assert_eq!(sent[0].parse_mode.as_deref(), Some("HTML"));
        assert!(sent[0].text.starts_with("<pre>"));
    }

    #[tokio::test]
    async fn handle_message_verbose_toggle() {
        let state = FakeTelegram::new(vec![Ok(()), Ok(())]);
        let ch = channel(&state);
        let (handle, _dir) = spawn_handle(vec![]);
        let mut verbose = false;

        handle_message(&ch, &handle, "/verbose", &mut verbose).await;
        assert!(verbose);
        let sent = state.sent_messages();
        assert_eq!(sent[0].text, "Verbose: on");

        handle_message(&ch, &handle, "/verbose", &mut verbose).await;
        assert!(!verbose);
        let sent = state.sent_messages();
        assert_eq!(sent[1].text, "Verbose: off");
    }

    #[tokio::test]
    async fn handle_message_unknown_command_sends_error() {
        let state = FakeTelegram::new(vec![Ok(())]);
        let ch = channel(&state);
        let (handle, _dir) = spawn_handle(vec![]);
        let mut verbose = false;

        handle_message(&ch, &handle, "/bogus", &mut verbose).await;

        let sent = state.sent_messages();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].text.contains("Unknown command"));
    }

    // -- poll_loop tests --

    #[tokio::test]
    async fn poll_loop_dispatches_valid_message() {
        tokio::time::pause();
        let state = FakeTelegram::new(vec![Ok(())]).with_poll_results(vec![vec![Update {
            update_id: 1,
            message: Some(TgMessage {
                message_id: 1,
                chat: Chat { id: CHAT_ID },
                text: Some("hello".into()),
            }),
        }]]);
        let ch = channel(&state);
        let (handle, _dir) = spawn_handle(vec![Ok(AgentResponse::Text("reply".into()))]);

        let _ = tokio::time::timeout(Duration::from_millis(100), poll_loop(&ch, &handle)).await;

        let sent = state.sent_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].text, "reply");
    }

    #[tokio::test]
    async fn poll_loop_filters_irrelevant_updates() {
        tokio::time::pause();
        let state = FakeTelegram::new(vec![]).with_poll_results(vec![vec![
            Update {
                update_id: 1,
                message: None,
            },
            Update {
                update_id: 2,
                message: Some(TgMessage {
                    message_id: 2,
                    chat: Chat { id: 999 },
                    text: Some("wrong chat".into()),
                }),
            },
            Update {
                update_id: 3,
                message: Some(TgMessage {
                    message_id: 3,
                    chat: Chat { id: CHAT_ID },
                    text: None,
                }),
            },
        ]]);
        let ch = channel(&state);
        let (handle, _dir) = spawn_handle(vec![]);

        let _ = tokio::time::timeout(Duration::from_millis(100), poll_loop(&ch, &handle)).await;

        assert!(state.sent_messages().is_empty());
    }

    #[tokio::test]
    async fn poll_loop_uses_configured_poll_timeout() {
        tokio::time::pause();
        let state = FakeTelegram::new(vec![Ok(())]).with_poll_results(vec![vec![Update {
            update_id: 1,
            message: Some(TgMessage {
                message_id: 1,
                chat: Chat { id: CHAT_ID },
                text: Some("hello".into()),
            }),
        }]]);
        let ch = TelegramChannel::new(fake_client(&state), CHAT_ID, 60);
        let (handle, _dir) = spawn_handle(vec![Ok(AgentResponse::Text("reply".into()))]);

        let _ = tokio::time::timeout(Duration::from_millis(100), poll_loop(&ch, &handle)).await;

        assert_eq!(state.last_poll_timeout(), Some(60));
    }
}
