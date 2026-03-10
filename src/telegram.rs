//! Telegram Bot API channel.
//!
//! Long-polls `getUpdates` for incoming messages, runs them through the
//! agent, and sends responses back via `sendMessage`. Designed to run as
//! an async loop alongside the heartbeat in the daemon's `tokio::select!`.
//!
//! Generic over [`TelegramApi`] so tests can substitute a mock api
//! without hitting the network.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::activity::Activity;
use crate::agent::TurnConfig;
use crate::clients::telegram::{RealTelegramApi, TelegramApi, TelegramClient};
use crate::dispatch;
use crate::error::TelegramError;
use crate::provider::Provider;
use crate::workspace::Workspace;

/// Concrete Telegram channel used by production code and the `mock-network` stub.
///
/// Internal helpers (`poll_loop`, `handle_message`) stay generic over
/// [`TelegramApi`] so that unit tests can inject [`FakeTelegramApi`].
pub type Telegram = TelegramChannel<RealTelegramApi>;

// --- Channel ---

/// Maximum retries for `sendMessage` on transient failures.
const SEND_RETRIES: u32 = 3;

/// Telegram Bot API channel.
///
/// Wraps a client implementing [`TelegramApi`] and the chat routing
/// configuration. The client handles raw HTTP; this struct layers retry
/// logic, HTML escaping, and message formatting on top.
pub struct TelegramChannel<A> {
    client: TelegramClient<A>,
    chat_id: i64,
}

impl<A: TelegramApi> TelegramChannel<A> {
    #[cfg_attr(feature = "mock-network", allow(dead_code))]
    pub fn new(client: TelegramClient<A>, chat_id: i64) -> Self {
        Self { client, chat_id }
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
            match self
                .client
                .post_message(self.chat_id, text, parse_mode)
                .await
            {
                Ok(()) => return Ok(()),
                Err(e) if attempts < SEND_RETRIES && is_transient(&e) => {
                    let delay = Duration::from_secs(u64::from(1u32 << attempts));
                    attempts += 1;
                    warn!(
                        attempt = attempts,
                        "send_message retrying in {delay:?}: {e}"
                    );
                    tokio::time::sleep(delay).await;
                }
                Err(e) => return Err(e),
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
        TelegramError::Session(_) => false,
    }
}

// --- Poll loop ---

/// Run the Telegram long-poll loop until cancelled.
///
/// This future never resolves normally — it loops forever, yielding at
/// each `getUpdates` call. The parent `tokio::select!` drops it on
/// shutdown.
pub async fn poll_loop<T: TelegramApi, P: Provider>(
    channel: &TelegramChannel<T>,
    workspace: &Workspace,
    config: &TurnConfig<'_, P>,
) -> ! {
    info!(chat_id = channel.chat_id(), "Telegram poller starting");
    let mut offset: i64 = 0;
    let mut verbose = false;
    let (tx, mut rx) = mpsc::channel(64);

    loop {
        let updates = match channel.client.poll_updates(offset, 30).await {
            Ok(updates) => updates,
            Err(TelegramError::Network(ref e)) => {
                error!("Telegram poll error: {e}");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
            Err(e) => {
                error!("Telegram API error: {e}");
                continue;
            }
        };

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

            handle_message(
                channel,
                workspace,
                config,
                &text,
                &mut verbose,
                &tx,
                &mut rx,
            )
            .await;
        }
    }
}

/// Process a single authorized text message.
async fn handle_message<T: TelegramApi, P: Provider>(
    channel: &TelegramChannel<T>,
    workspace: &Workspace,
    config: &TurnConfig<'_, P>,
    text: &str,
    verbose: &mut bool,
    tx: &mpsc::Sender<Activity>,
    rx: &mut mpsc::Receiver<Activity>,
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

    let session_path = workspace.telegram_session_path();

    let cancel = CancellationToken::new();
    let result = {
        let dispatch_fut =
            dispatch::dispatch(trimmed, &session_path, workspace, config, Some(tx), &cancel);
        tokio::pin!(dispatch_fut);

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
                result = &mut dispatch_fut => break result,
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
    use crate::agent::TurnConfig;
    use crate::clients::telegram::{GetUpdatesBody, SendMessageBody};
    use crate::config::ContextConfig;
    use crate::provider::MockProvider;
    use crate::tools::Tools;
    use crate::types::Response as AgentResponse;
    use crate::workspace::Workspace;

    // -- Fake TelegramApi for channel tests --

    /// A captured `post_message` call.
    #[derive(Clone, Debug)]
    struct SentMessage {
        chat_id: i64,
        text: String,
        parse_mode: Option<String>,
    }

    /// Fake Telegram API that captures outgoing messages and returns
    /// pre-configured results.
    ///
    /// Uses `Arc` internally so cloning shares state — clone before
    /// moving into the channel, then inspect the original.
    ///
    /// `poll_updates` pops from the poll queue; when empty it returns
    /// a permanently pending future so `tokio::select!` can cancel
    /// the loop via a timeout.
    #[derive(Clone)]
    struct FakeTelegramApi {
        poll_results: Arc<Mutex<VecDeque<serde_json::Value>>>,
        send_results: Arc<Vec<Result<(), TelegramError>>>,
        send_index: Arc<AtomicUsize>,
        sent: Arc<Mutex<Vec<SentMessage>>>,
    }

    impl FakeTelegramApi {
        fn new(send_results: Vec<Result<(), TelegramError>>) -> Self {
            Self {
                poll_results: Arc::new(Mutex::new(VecDeque::new())),
                send_results: Arc::new(send_results),
                send_index: Arc::new(AtomicUsize::new(0)),
                sent: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn with_poll_results(mut self, results: Vec<serde_json::Value>) -> Self {
            self.poll_results = Arc::new(Mutex::new(results.into()));
            self
        }

        fn sent_messages(&self) -> Vec<SentMessage> {
            self.sent.lock().unwrap().clone()
        }

        fn json_response(json: &serde_json::Value) -> reqwest::Response {
            reqwest::Response::from(
                http::Response::builder()
                    .status(200)
                    .header("content-type", "application/json")
                    .body(json.to_string())
                    .unwrap(),
            )
        }

        fn ok_response(result: &serde_json::Value) -> reqwest::Response {
            Self::json_response(&serde_json::json!({ "ok": true, "result": result }))
        }

        fn error_response(error_code: i32, description: &str) -> reqwest::Response {
            Self::json_response(&serde_json::json!({
                "ok": false,
                "error_code": error_code,
                "description": description,
            }))
        }
    }

    impl TelegramApi for FakeTelegramApi {
        async fn poll_updates(
            &self,
            _body: GetUpdatesBody,
        ) -> Result<reqwest::Response, reqwest::Error> {
            let next = self.poll_results.lock().unwrap().pop_front();
            match next {
                Some(updates) => Ok(Self::ok_response(&updates)),
                None => std::future::pending().await,
            }
        }

        async fn post_message(
            &self,
            body: SendMessageBody<'_>,
        ) -> Result<reqwest::Response, reqwest::Error> {
            let index = self.send_index.fetch_add(1, Ordering::SeqCst);
            self.sent.lock().unwrap().push(SentMessage {
                chat_id: body.chat_id,
                text: body.text.to_string(),
                parse_mode: body.parse_mode.map(str::to_string),
            });

            match &self.send_results[index] {
                Ok(()) => Ok(Self::ok_response(&serde_json::json!({"message_id": 1}))),
                Err(TelegramError::Api {
                    error_code,
                    description,
                }) => Ok(Self::error_response(*error_code, description)),
                Err(TelegramError::Network(_)) => {
                    let resp = reqwest::Response::from(
                        http::Response::builder().status(500).body("").unwrap(),
                    );
                    Err(resp.error_for_status().unwrap_err())
                }
                Err(TelegramError::Session(msg)) => Ok(Self::error_response(0, msg)),
            }
        }
    }

    // -- Helpers --

    const CHAT_ID: i64 = 42;

    const CTX: ContextConfig = ContextConfig {
        max_tokens: 200_000,
        budget_percent: 80,
    };

    fn channel(api: FakeTelegramApi) -> TelegramChannel<FakeTelegramApi> {
        TelegramChannel::new(TelegramClient::new(api), CHAT_ID)
    }

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::init_at(dir.path().to_path_buf()).unwrap();
        (dir, ws)
    }

    // -- Pure unit tests --

    #[test]
    fn transient_error_classification() {
        assert!(is_transient(&TelegramError::Api {
            error_code: 500,
            description: "Internal Server Error".into(),
        }));
        assert!(is_transient(&TelegramError::Api {
            error_code: 429,
            description: "Too Many Requests".into(),
        }));
        assert!(!is_transient(&TelegramError::Api {
            error_code: 400,
            description: "Bad Request".into(),
        }));
        assert!(!is_transient(&TelegramError::Session("test".into())));
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
        let api = FakeTelegramApi::new(vec![Ok(())]);
        let ch = channel(api.clone());

        ch.send_message("hello").await.unwrap();

        let sent = api.sent_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].chat_id, CHAT_ID);
        assert_eq!(sent[0].text, "hello");
        assert!(sent[0].parse_mode.is_none());
    }

    #[tokio::test]
    async fn send_raw_retries_on_transient_then_succeeds() {
        tokio::time::pause();

        let api = FakeTelegramApi::new(vec![
            Err(TelegramError::Network("timeout".into())),
            Err(TelegramError::Api {
                error_code: 429,
                description: "Too Many Requests".into(),
            }),
            Ok(()),
        ]);
        let ch = channel(api.clone());

        ch.send_message("retry me").await.unwrap();

        assert_eq!(api.sent_messages().len(), 3);
    }

    #[tokio::test]
    async fn send_raw_does_not_retry_non_transient_error() {
        let api = FakeTelegramApi::new(vec![Err(TelegramError::Api {
            error_code: 400,
            description: "Bad Request".into(),
        })]);
        let ch = channel(api.clone());

        let err = ch.send_message("bad").await.unwrap_err();

        assert_eq!(api.sent_messages().len(), 1);
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
        let api = FakeTelegramApi::new(vec![
            Err(TelegramError::Network("1".into())),
            Err(TelegramError::Network("2".into())),
            Err(TelegramError::Network("3".into())),
            Err(TelegramError::Network("4".into())),
        ]);
        let ch = channel(api.clone());

        let err = ch.send_message("doomed").await.unwrap_err();

        // 1 initial + 3 retries = 4 attempts total
        assert_eq!(api.sent_messages().len(), 4);
        assert!(matches!(err, TelegramError::Network(_)));
    }

    #[tokio::test]
    async fn send_preformatted_escapes_html() {
        let api = FakeTelegramApi::new(vec![Ok(())]);
        let ch = channel(api.clone());

        ch.send_preformatted("a < b & c > d").await.unwrap();

        let sent = api.sent_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].text, "<pre>a &lt; b &amp; c &gt; d</pre>");
        assert_eq!(sent[0].parse_mode.as_deref(), Some("HTML"));
    }

    // -- handle_message tests --

    #[tokio::test]
    async fn handle_message_dispatches_and_sends_reply() {
        let (_dir, ws) = workspace();
        let api = FakeTelegramApi::new(vec![Ok(())]);
        let ch = channel(api.clone());
        let provider = MockProvider::new(vec![Ok(AgentResponse::Text("pong".into()))]);
        let tools = Tools::default();
        let config = TurnConfig {
            provider: &provider,
            tools: &tools,
            max_iterations: 1,
            context: &CTX,
        };
        let (tx, mut rx) = mpsc::channel(64);
        let mut verbose = false;

        handle_message(&ch, &ws, &config, "ping", &mut verbose, &tx, &mut rx).await;

        let sent = api.sent_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].text, "pong");
        assert!(sent[0].parse_mode.is_none());
    }

    #[tokio::test]
    async fn handle_message_sends_preformatted_for_preformatted_reply() {
        let (_dir, ws) = workspace();
        // /stats produces a preformatted reply.
        let api = FakeTelegramApi::new(vec![Ok(())]);
        let ch = channel(api.clone());
        let provider = MockProvider::new(vec![]);
        let tools = Tools::default();
        let config = TurnConfig {
            provider: &provider,
            tools: &tools,
            max_iterations: 1,
            context: &CTX,
        };
        let (tx, mut rx) = mpsc::channel(64);
        let mut verbose = false;

        handle_message(&ch, &ws, &config, "/stats", &mut verbose, &tx, &mut rx).await;

        let sent = api.sent_messages();
        assert_eq!(sent.len(), 1);
        // /stats returns preformatted, so it's HTML-escaped and wrapped in <pre>
        assert_eq!(sent[0].parse_mode.as_deref(), Some("HTML"));
        assert!(sent[0].text.starts_with("<pre>"));
    }

    #[tokio::test]
    async fn handle_message_verbose_toggle() {
        let (_dir, ws) = workspace();
        let api = FakeTelegramApi::new(vec![Ok(()), Ok(())]);
        let ch = channel(api.clone());
        let provider = MockProvider::new(vec![]);
        let tools = Tools::default();
        let config = TurnConfig {
            provider: &provider,
            tools: &tools,
            max_iterations: 1,
            context: &CTX,
        };
        let (tx, mut rx) = mpsc::channel(64);
        let mut verbose = false;

        handle_message(&ch, &ws, &config, "/verbose", &mut verbose, &tx, &mut rx).await;
        assert!(verbose);
        let sent = api.sent_messages();
        assert_eq!(sent[0].text, "Verbose: on");

        handle_message(&ch, &ws, &config, "/verbose", &mut verbose, &tx, &mut rx).await;
        assert!(!verbose);
        let sent = api.sent_messages();
        assert_eq!(sent[1].text, "Verbose: off");

        // Provider was never called — /verbose is intercepted before dispatch.
        assert_eq!(provider.call_count(), 0);
    }

    #[tokio::test]
    async fn handle_message_unknown_command_sends_error() {
        let (_dir, ws) = workspace();
        let api = FakeTelegramApi::new(vec![Ok(())]);
        let ch = channel(api.clone());
        let provider = MockProvider::new(vec![]);
        let tools = Tools::default();
        let config = TurnConfig {
            provider: &provider,
            tools: &tools,
            max_iterations: 1,
            context: &CTX,
        };
        let (tx, mut rx) = mpsc::channel(64);
        let mut verbose = false;

        handle_message(&ch, &ws, &config, "/bogus", &mut verbose, &tx, &mut rx).await;

        let sent = api.sent_messages();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].text.contains("Unknown command"));
    }

    // -- poll_loop tests --

    #[tokio::test]
    async fn poll_loop_dispatches_valid_message() {
        tokio::time::pause();
        let (_dir, ws) = workspace();
        let api = FakeTelegramApi::new(vec![Ok(())]).with_poll_results(vec![serde_json::json!([{
            "update_id": 1,
            "message": { "chat": {"id": CHAT_ID}, "text": "hello" }
        }])]);
        let ch = channel(api.clone());
        let provider = MockProvider::new(vec![Ok(AgentResponse::Text("reply".into()))]);
        let tools = Tools::default();
        let config = TurnConfig {
            provider: &provider,
            tools: &tools,
            max_iterations: 1,
            context: &CTX,
        };

        let _ =
            tokio::time::timeout(Duration::from_millis(100), poll_loop(&ch, &ws, &config)).await;

        let sent = api.sent_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].text, "reply");
    }

    #[tokio::test]
    async fn poll_loop_filters_irrelevant_updates() {
        tokio::time::pause();
        let (_dir, ws) = workspace();
        let api = FakeTelegramApi::new(vec![]).with_poll_results(vec![serde_json::json!([
            {"update_id": 1},
            {"update_id": 2, "message": {"chat": {"id": 999}, "text": "wrong chat"}},
            {"update_id": 3, "message": {"chat": {"id": CHAT_ID}}}
        ])]);
        let ch = channel(api.clone());
        let provider = MockProvider::new(vec![]);
        let tools = Tools::default();
        let config = TurnConfig {
            provider: &provider,
            tools: &tools,
            max_iterations: 1,
            context: &CTX,
        };

        let _ =
            tokio::time::timeout(Duration::from_millis(100), poll_loop(&ch, &ws, &config)).await;

        assert!(api.sent_messages().is_empty());
        assert_eq!(provider.call_count(), 0);
    }
}
