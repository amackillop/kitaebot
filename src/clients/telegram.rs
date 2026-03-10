//! Telegram Bot API client.
//!
//! Thin wrapper around `reqwest` that handles authentication and the
//! `getUpdates` / `sendMessage` endpoints. Follows the same layering as
//! [`CompletionsApi`](super::chat_completion::CompletionsApi): a trait
//! for the network boundary, a real HTTP client, a `mock-network` stub,
//! and a test mock.

#[cfg(not(feature = "mock-network"))]
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::error::TelegramError;
#[cfg(not(feature = "mock-network"))]
use crate::secrets::Secret;

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Abstraction over the Telegram Bot API HTTP calls.
///
/// Implemented by the real HTTP client, the `mock-network` stub, and the
/// test mock. [`TelegramChannel`](crate::telegram::TelegramChannel) is
/// generic over this trait so that tests can exercise the real
/// retry/formatting code without hitting the network.
pub trait TelegramApi: Send + Sync {
    /// Long-poll `getUpdates` for new messages.
    async fn poll_updates(&self, offset: i64) -> Result<Vec<Update>, TelegramError>;

    /// Send a single message via `sendMessage`.
    async fn post_message(
        &self,
        chat_id: i64,
        text: &str,
        parse_mode: Option<&str>,
    ) -> Result<(), TelegramError>;
}

// ---------------------------------------------------------------------------
// Real client
// ---------------------------------------------------------------------------

/// HTTP client for the Telegram Bot API.
#[cfg(not(feature = "mock-network"))]
pub struct TelegramClient {
    client: Client,
    token: Secret,
    poll_timeout: u64,
}

#[cfg(not(feature = "mock-network"))]
impl TelegramClient {
    pub fn new(token: Secret, poll_timeout: u64) -> Self {
        use std::time::Duration;

        let client = Client::builder()
            .timeout(Duration::from_secs(poll_timeout + 10))
            .build()
            .expect("failed to build HTTP client");
        Self {
            client,
            token,
            poll_timeout,
        }
    }

    fn api_url(&self, method: &str) -> String {
        format!(
            "https://api.telegram.org/bot{}/{}",
            self.token.expose(),
            method,
        )
    }
}

#[cfg(not(feature = "mock-network"))]
impl TelegramApi for TelegramClient {
    async fn poll_updates(&self, offset: i64) -> Result<Vec<Update>, TelegramError> {
        let body = GetUpdatesBody {
            offset,
            timeout: self.poll_timeout,
        };
        let resp: ApiResponse<Vec<Update>> = self
            .client
            .post(self.api_url("getUpdates"))
            .json(&body)
            .send()
            .await
            .map_err(|e| TelegramError::Network(e.to_string()))?
            .json()
            .await
            .map_err(|e| TelegramError::Network(e.to_string()))?;
        resp.into_result()
    }

    async fn post_message(
        &self,
        chat_id: i64,
        text: &str,
        parse_mode: Option<&str>,
    ) -> Result<(), TelegramError> {
        let body = SendMessageBody {
            chat_id,
            text,
            parse_mode,
        };
        let resp: ApiResponse<serde_json::Value> = self
            .client
            .post(self.api_url("sendMessage"))
            .json(&body)
            .send()
            .await
            .map_err(|e| TelegramError::Network(e.to_string()))?
            .json()
            .await
            .map_err(|e| TelegramError::Network(e.to_string()))?;
        resp.into_result().map(|_| ())
    }
}

// ---------------------------------------------------------------------------
// Stub client (mock-network builds)
// ---------------------------------------------------------------------------

#[cfg(feature = "mock-network")]
pub struct TelegramClient;

#[cfg(feature = "mock-network")]
impl TelegramApi for TelegramClient {
    async fn poll_updates(&self, _offset: i64) -> Result<Vec<Update>, TelegramError> {
        Ok(vec![])
    }

    async fn post_message(
        &self,
        _chat_id: i64,
        _text: &str,
        _parse_mode: Option<&str>,
    ) -> Result<(), TelegramError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Wire format types (Telegram Bot API)
// ---------------------------------------------------------------------------

// Intentionally no `deny_unknown_fields` — Telegram returns many fields
// we don't care about, and the API grows over time.

/// Wrapper returned by every Bot API method.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "mock-network", allow(dead_code))]
pub(crate) struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    error_code: Option<i32>,
    description: Option<String>,
}

#[cfg_attr(feature = "mock-network", allow(dead_code))]
impl<T> ApiResponse<T> {
    pub(crate) fn into_result(self) -> Result<T, TelegramError> {
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
#[derive(Clone, Debug, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<TgMessage>,
}

/// A Telegram message.
#[derive(Clone, Debug, Deserialize)]
pub struct TgMessage {
    pub chat: Chat,
    pub text: Option<String>,
}

/// Identifies the conversation a message belongs to.
#[derive(Clone, Debug, Deserialize)]
pub struct Chat {
    pub id: i64,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "mock-network", allow(dead_code))]
pub(crate) struct GetUpdatesBody {
    pub(crate) offset: i64,
    pub(crate) timeout: u64,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "mock-network", allow(dead_code))]
pub(crate) struct SendMessageBody<'a> {
    pub(crate) chat_id: i64,
    pub(crate) text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parse_mode: Option<&'a str>,
}

// ---------------------------------------------------------------------------
// Test mock
// ---------------------------------------------------------------------------

#[cfg(test)]
pub mod mock {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A captured `post_message` call.
    #[derive(Clone, Debug)]
    pub struct SentMessage {
        pub chat_id: i64,
        pub text: String,
        pub parse_mode: Option<String>,
    }

    /// Mock client that returns pre-configured `post_message` results
    /// and captures every call for later inspection.
    ///
    /// `poll_updates` always returns `Ok(vec![])` — poll-loop tests
    /// belong in integration tests that drive the full daemon.
    ///
    /// Uses `Arc` internally so cloning shares state.
    #[derive(Clone)]
    pub struct MockTelegramClient {
        send_results: Arc<Vec<Result<(), TelegramError>>>,
        send_index: Arc<AtomicUsize>,
        sent: Arc<std::sync::Mutex<Vec<SentMessage>>>,
    }

    impl MockTelegramClient {
        pub fn new(send_results: Vec<Result<(), TelegramError>>) -> Self {
            Self {
                send_results: Arc::new(send_results),
                send_index: Arc::new(AtomicUsize::new(0)),
                sent: Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }

        /// Return a snapshot of all captured `post_message` calls.
        pub fn sent_messages(&self) -> Vec<SentMessage> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl TelegramApi for MockTelegramClient {
        async fn poll_updates(&self, _offset: i64) -> Result<Vec<Update>, TelegramError> {
            Ok(vec![])
        }

        async fn post_message(
            &self,
            chat_id: i64,
            text: &str,
            parse_mode: Option<&str>,
        ) -> Result<(), TelegramError> {
            let index = self.send_index.fetch_add(1, Ordering::SeqCst);
            self.sent.lock().unwrap().push(SentMessage {
                chat_id,
                text: text.to_string(),
                parse_mode: parse_mode.map(str::to_string),
            });
            self.send_results[index].clone()
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (wire format)
// ---------------------------------------------------------------------------

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
    fn send_body_serialization() {
        let body = SendMessageBody {
            chat_id: 123,
            text: "hello",
            parse_mode: None,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["chat_id"], 123);
        assert_eq!(json["text"], "hello");
        assert!(json.get("parse_mode").is_none());
    }

    #[test]
    fn send_body_with_parse_mode() {
        let body = SendMessageBody {
            chat_id: 123,
            text: "<pre>hi</pre>",
            parse_mode: Some("HTML"),
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["parse_mode"], "HTML");
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
