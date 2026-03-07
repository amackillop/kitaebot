//! Telegram Bot API channel.
//!
//! Long-polls `getUpdates` for incoming messages, runs them through the
//! agent, and sends responses back via `sendMessage`. Designed to run as
//! an async loop alongside the heartbeat in the daemon's `tokio::select!`.

use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

use crate::agent::TurnConfig;
use crate::commands;
use crate::config::TelegramConfig;
use crate::error::TelegramError;
use crate::provider::Provider;
use crate::secrets::Secret;
use crate::workspace::Workspace;

// --- Telegram Bot API types ---
//
// Intentionally no `deny_unknown_fields` — Telegram returns many fields
// we don't care about, and the API grows over time.

/// Wrapper returned by every Bot API method.
#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    error_code: Option<i32>,
    description: Option<String>,
}

impl<T> ApiResponse<T> {
    fn into_result(self) -> Result<T, TelegramError> {
        if self.ok {
            self.result.ok_or_else(|| TelegramError::Api {
                error_code: 0,
                description: "ok=true but missing result".into(),
            })
        } else {
            Err(TelegramError::Api {
                error_code: self.error_code.unwrap_or(0),
                description: self.description.unwrap_or_default(),
            })
        }
    }
}

/// A single incoming update from `getUpdates`.
#[derive(Debug, Deserialize)]
struct Update {
    update_id: i64,
    message: Option<TgMessage>,
}

/// A Telegram message.
#[derive(Debug, Deserialize)]
struct TgMessage {
    chat: Chat,
    text: Option<String>,
}

/// Identifies the conversation a message belongs to.
#[derive(Debug, Deserialize)]
struct Chat {
    id: i64,
}

#[derive(Debug, Serialize)]
struct GetUpdatesBody {
    offset: i64,
    timeout: u64,
}

#[derive(Debug, Serialize)]
struct SendMessageBody<'a> {
    chat_id: i64,
    text: &'a str,
}

// --- Channel ---

/// Maximum retries for `sendMessage` on transient failures.
const SEND_RETRIES: u32 = 3;

/// Telegram Bot API long-polling client.
///
/// Owns the HTTP client, bot token, and chat routing configuration.
/// Construct via [`TelegramChannel::new`], then drive with [`poll_loop`].
pub struct TelegramChannel {
    client: Client,
    token: Secret,
    chat_id: i64,
    poll_timeout: u64,
}

impl TelegramChannel {
    /// Build a channel from a pre-loaded token.
    #[cfg_attr(feature = "mock-network", allow(dead_code))]
    pub fn new(token: Secret, config: &TelegramConfig) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(config.poll_timeout_secs + 10))
            .build()
            .expect("failed to build HTTP client");
        Self {
            client,
            token,
            chat_id: config.chat_id,
            poll_timeout: config.poll_timeout_secs,
        }
    }

    fn api_url(&self, method: &str) -> String {
        format!(
            "https://api.telegram.org/bot{}/{}",
            self.token.expose(),
            method,
        )
    }

    async fn get_updates(&self, offset: i64) -> Result<Vec<Update>, TelegramError> {
        let body = GetUpdatesBody {
            offset,
            timeout: self.poll_timeout,
        };
        let resp: ApiResponse<Vec<Update>> = self
            .client
            .post(self.api_url("getUpdates"))
            .json(&body)
            .send()
            .await?
            .json()
            .await?;
        resp.into_result()
    }

    /// Send a text message with retries on transient failures.
    ///
    /// Retries up to [`SEND_RETRIES`] times with exponential backoff
    /// (1s, 2s, 4s) on network errors and 429/5xx API responses.
    async fn send_message(&self, text: &str) -> Result<(), TelegramError> {
        let body = SendMessageBody {
            chat_id: self.chat_id,
            text,
        };
        let mut attempts = 0u32;
        loop {
            match self.try_send(&body).await {
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

    async fn try_send(&self, body: &SendMessageBody<'_>) -> Result<(), TelegramError> {
        let resp: ApiResponse<serde_json::Value> = self
            .client
            .post(self.api_url("sendMessage"))
            .json(body)
            .send()
            .await?
            .json()
            .await?;
        resp.into_result().map(|_| ())
    }
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
pub async fn poll_loop<P: Provider>(
    channel: &TelegramChannel,
    workspace: &Workspace,
    config: &TurnConfig<'_, P>,
) -> ! {
    info!(chat_id = channel.chat_id, "Telegram poller starting");
    let mut offset: i64 = 0;

    loop {
        let updates = match channel.get_updates(offset).await {
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

            if message.chat.id != channel.chat_id {
                warn!(
                    chat_id = message.chat.id,
                    expected = channel.chat_id,
                    "Message from unauthorized chat, skipping",
                );
                continue;
            }

            let Some(text) = message.text else {
                debug!("Non-text message, skipping");
                continue;
            };

            handle_message(channel, workspace, config, &text).await;
        }
    }
}

/// Process a single authorized text message.
async fn handle_message<P: Provider>(
    channel: &TelegramChannel,
    workspace: &Workspace,
    config: &TurnConfig<'_, P>,
    text: &str,
) {
    let session_path = workspace.telegram_session_path();

    let reply = match commands::dispatch(text, &session_path, workspace, config).await {
        Ok(response) => response,
        Err(msg) => msg,
    };

    if let Err(e) = channel.send_message(&reply).await {
        error!("Failed to send response: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_success_response() {
        let json = r#"{
            "ok": true,
            "result": [{
                "update_id": 1,
                "message": {
                    "message_id": 1,
                    "chat": {"id": 123, "type": "private"},
                    "date": 1234567890,
                    "text": "hello"
                }
            }]
        }"#;
        let resp: ApiResponse<Vec<Update>> = serde_json::from_str(json).unwrap();
        let updates = resp.into_result().unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].update_id, 1);
        let msg = updates[0].message.as_ref().unwrap();
        assert_eq!(msg.chat.id, 123);
        assert_eq!(msg.text.as_deref(), Some("hello"));
    }

    #[test]
    fn deserialize_error_response() {
        let json = r#"{"ok": false, "error_code": 401, "description": "Unauthorized"}"#;
        let resp: ApiResponse<Vec<Update>> = serde_json::from_str(json).unwrap();
        let err = resp.into_result().unwrap_err();
        match err {
            TelegramError::Api {
                error_code,
                description,
            } => {
                assert_eq!(error_code, 401);
                assert_eq!(description, "Unauthorized");
            }
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[test]
    fn deserialize_update_without_message() {
        let json = r#"{"ok": true, "result": [{"update_id": 42}]}"#;
        let resp: ApiResponse<Vec<Update>> = serde_json::from_str(json).unwrap();
        let updates = resp.into_result().unwrap();
        assert_eq!(updates.len(), 1);
        assert!(updates[0].message.is_none());
    }

    #[test]
    fn deserialize_message_without_text() {
        let json = r#"{
            "ok": true,
            "result": [{
                "update_id": 1,
                "message": {
                    "message_id": 1,
                    "chat": {"id": 123, "type": "private"},
                    "date": 1234567890
                }
            }]
        }"#;
        let resp: ApiResponse<Vec<Update>> = serde_json::from_str(json).unwrap();
        let updates = resp.into_result().unwrap();
        assert!(updates[0].message.as_ref().unwrap().text.is_none());
    }

    #[test]
    fn deserialize_ignores_unknown_fields() {
        let json = r#"{
            "ok": true,
            "result": [{
                "update_id": 1,
                "message": {
                    "message_id": 1,
                    "from": {"id": 456, "is_bot": false, "first_name": "Test"},
                    "chat": {"id": 123, "type": "private", "first_name": "Test"},
                    "date": 1234567890,
                    "text": "hi",
                    "entities": []
                }
            }]
        }"#;
        let resp: ApiResponse<Vec<Update>> = serde_json::from_str(json).unwrap();
        let updates = resp.into_result().unwrap();
        assert_eq!(
            updates[0].message.as_ref().unwrap().text.as_deref(),
            Some("hi")
        );
    }

    #[test]
    fn deserialize_empty_result() {
        let json = r#"{"ok": true, "result": []}"#;
        let resp: ApiResponse<Vec<Update>> = serde_json::from_str(json).unwrap();
        let updates = resp.into_result().unwrap();
        assert!(updates.is_empty());
    }

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
    fn send_body_serialization() {
        let body = SendMessageBody {
            chat_id: 123,
            text: "hello",
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["chat_id"], 123);
        assert_eq!(json["text"], "hello");
    }

    #[test]
    fn get_updates_body_serialization() {
        let body = GetUpdatesBody {
            offset: 42,
            timeout: 30,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["offset"], 42);
        assert_eq!(json["timeout"], 30);
    }
}
