//! Telegram Bot API channel.
//!
//! Long-polls `getUpdates` for incoming messages, runs them through the
//! agent, and sends responses back via `sendMessage`. Designed to run as
//! an async loop alongside the heartbeat in the daemon's `tokio::select!`.
//!
//! Generic over [`TelegramApi`] so tests can substitute a mock client
//! without hitting the network.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::activity::Activity;
use crate::agent::TurnConfig;
use crate::clients::telegram::TelegramApi;
use crate::dispatch;
use crate::error::TelegramError;
use crate::provider::Provider;
use crate::workspace::Workspace;

// --- Channel ---

/// Maximum retries for `sendMessage` on transient failures.
const SEND_RETRIES: u32 = 3;

/// Telegram Bot API channel.
///
/// Wraps a client implementing [`TelegramApi`] and the chat routing
/// configuration. The client handles raw HTTP; this struct layers retry
/// logic, HTML escaping, and message formatting on top.
pub struct TelegramChannel<T> {
    client: T,
    chat_id: i64,
}

impl<T: TelegramApi> TelegramChannel<T> {
    #[cfg_attr(feature = "mock-network", allow(dead_code))]
    pub fn new(client: T, chat_id: i64) -> Self {
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
        let updates = match channel.client.poll_updates(offset).await {
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
    use super::*;
    use crate::agent::TurnConfig;
    use crate::clients::telegram::mock::MockTelegramClient;
    use crate::config::ContextConfig;
    use crate::provider::MockProvider;
    use crate::tools::Tools;
    use crate::types::Response;
    use crate::workspace::Workspace;

    const CHAT_ID: i64 = 42;

    const CTX: ContextConfig = ContextConfig {
        max_tokens: 200_000,
        budget_percent: 80,
    };

    fn channel(client: MockTelegramClient) -> TelegramChannel<MockTelegramClient> {
        TelegramChannel::new(client, CHAT_ID)
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
        let client = MockTelegramClient::new(vec![Ok(())]);
        let ch = channel(client.clone());

        ch.send_message("hello").await.unwrap();

        let sent = client.sent_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].chat_id, CHAT_ID);
        assert_eq!(sent[0].text, "hello");
        assert!(sent[0].parse_mode.is_none());
    }

    #[tokio::test]
    async fn send_raw_retries_on_transient_then_succeeds() {
        tokio::time::pause();

        let client = MockTelegramClient::new(vec![
            Err(TelegramError::Network("timeout".into())),
            Err(TelegramError::Api {
                error_code: 429,
                description: "Too Many Requests".into(),
            }),
            Ok(()),
        ]);
        let ch = channel(client.clone());

        ch.send_message("retry me").await.unwrap();

        assert_eq!(client.sent_messages().len(), 3);
    }

    #[tokio::test]
    async fn send_raw_does_not_retry_non_transient_error() {
        let client = MockTelegramClient::new(vec![Err(TelegramError::Api {
            error_code: 400,
            description: "Bad Request".into(),
        })]);
        let ch = channel(client.clone());

        let err = ch.send_message("bad").await.unwrap_err();

        assert_eq!(client.sent_messages().len(), 1);
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
        let client = MockTelegramClient::new(vec![
            Err(TelegramError::Network("1".into())),
            Err(TelegramError::Network("2".into())),
            Err(TelegramError::Network("3".into())),
            Err(TelegramError::Network("4".into())),
        ]);
        let ch = channel(client.clone());

        let err = ch.send_message("doomed").await.unwrap_err();

        // 1 initial + 3 retries = 4 attempts total
        assert_eq!(client.sent_messages().len(), 4);
        assert!(matches!(err, TelegramError::Network(_)));
    }

    #[tokio::test]
    async fn send_preformatted_escapes_html() {
        let client = MockTelegramClient::new(vec![Ok(())]);
        let ch = channel(client.clone());

        ch.send_preformatted("a < b & c > d").await.unwrap();

        let sent = client.sent_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].text, "<pre>a &lt; b &amp; c &gt; d</pre>");
        assert_eq!(sent[0].parse_mode.as_deref(), Some("HTML"));
    }

    // -- handle_message tests --

    #[tokio::test]
    async fn handle_message_dispatches_and_sends_reply() {
        let (_dir, ws) = workspace();
        let tg = MockTelegramClient::new(vec![Ok(())]);
        let ch = channel(tg.clone());
        let provider = MockProvider::new(vec![Ok(Response::Text("pong".into()))]);
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

        let sent = tg.sent_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].text, "pong");
        assert!(sent[0].parse_mode.is_none());
    }

    #[tokio::test]
    async fn handle_message_sends_preformatted_for_preformatted_reply() {
        let (_dir, ws) = workspace();
        // /stats produces a preformatted reply.
        let tg = MockTelegramClient::new(vec![Ok(())]);
        let ch = channel(tg.clone());
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

        let sent = tg.sent_messages();
        assert_eq!(sent.len(), 1);
        // /stats returns preformatted, so it's HTML-escaped and wrapped in <pre>
        assert_eq!(sent[0].parse_mode.as_deref(), Some("HTML"));
        assert!(sent[0].text.starts_with("<pre>"));
    }

    #[tokio::test]
    async fn handle_message_verbose_toggle() {
        let (_dir, ws) = workspace();
        let tg = MockTelegramClient::new(vec![Ok(()), Ok(())]);
        let ch = channel(tg.clone());
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
        let sent = tg.sent_messages();
        assert_eq!(sent[0].text, "Verbose: on");

        handle_message(&ch, &ws, &config, "/verbose", &mut verbose, &tx, &mut rx).await;
        assert!(!verbose);
        let sent = tg.sent_messages();
        assert_eq!(sent[1].text, "Verbose: off");

        // Provider was never called — /verbose is intercepted before dispatch.
        assert_eq!(provider.call_count(), 0);
    }

    #[tokio::test]
    async fn handle_message_unknown_command_sends_error() {
        let (_dir, ws) = workspace();
        let tg = MockTelegramClient::new(vec![Ok(())]);
        let ch = channel(tg.clone());
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

        let sent = tg.sent_messages();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].text.contains("Unknown command"));
    }
}
