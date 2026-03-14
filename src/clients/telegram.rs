//! Telegram Bot API client.
//!
//! Pure response parsing lives in [`interpret_response`]. The IO layer is a
//! stored closure inside [`TelegramClient`] — swap it for tests or
//! `mock-network` builds without traits or generics.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::RawResponse;
use crate::error::TelegramError;
use crate::secrets::Secret;

// ---------------------------------------------------------------------------
// Closure type alias
// ---------------------------------------------------------------------------

type PostResult = Result<RawResponse, TelegramError>;
type PostFuture = Pin<Box<dyn Future<Output = PostResult> + Send>>;
type PostFn = Arc<dyn Fn(String, Vec<u8>) -> PostFuture + Send + Sync>;

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// HTTP client for the Telegram Bot API.
///
/// Concrete struct — no generics. The IO strategy is a closure injected at
/// construction time. `Clone` is free (`Arc`).
#[derive(Clone)]
pub struct TelegramClient {
    post: PostFn,
}

impl TelegramClient {
    pub fn new(bot_token: Secret, timeout: Duration) -> Self {
        #[cfg(not(feature = "mock-network"))]
        {
            const BASE_URL: &str = "https://api.telegram.org/bot";
            let client = reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .expect("failed to build HTTP client");
            Self {
                post: Arc::new(move |method, body| {
                    let client = client.clone();
                    let url = format!("{BASE_URL}{}/{method}", bot_token.expose());
                    Box::pin(async move {
                        let resp = client
                            .post(&url)
                            .header("Content-Type", "application/json")
                            .body(body)
                            .send()
                            .await
                            .map_err(|e| TelegramError::Network(e.to_string()))?;
                        let status = resp.status().as_u16();
                        let bytes = resp
                            .bytes()
                            .await
                            .map_err(|e| TelegramError::Network(e.to_string()))?;
                        Ok(RawResponse {
                            status,
                            body: bytes.to_vec(),
                        })
                    })
                }),
            }
        }
        #[cfg(feature = "mock-network")]
        {
            let _ = (bot_token, timeout);
            Self {
                post: Arc::new(|method, _body| {
                    Box::pin(async move {
                        let body = match method.as_str() {
                            "getUpdates" => br#"{"ok":true,"result":[]}"#.as_slice(),
                            _ => br#"{"ok":true,"result":{"message_id":1,"chat":{"id":0},"text":null}}"#.as_slice(),
                        };
                        Ok(RawResponse {
                            status: 200,
                            body: body.to_vec(),
                        })
                    })
                }),
            }
        }
    }

    /// Test constructor — inject an arbitrary closure.
    #[cfg(test)]
    pub fn from_fn<F, Fut>(f: F) -> Self
    where
        F: Fn(String, Vec<u8>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = PostResult> + Send + 'static,
    {
        Self {
            post: Arc::new(move |method, body| Box::pin(f(method, body))),
        }
    }

    pub async fn poll_updates(
        &self,
        offset: i64,
        timeout: u64,
    ) -> Result<Vec<Update>, TelegramError> {
        let body = GetUpdatesBody { offset, timeout };
        let raw_body =
            serde_json::to_vec(&body).map_err(|e| TelegramError::Network(e.to_string()))?;
        let raw = (self.post)("getUpdates".into(), raw_body).await?;
        interpret_response(&raw)
    }

    pub async fn post_message(
        &self,
        chat_id: i64,
        text: &str,
        parse_mode: Option<&str>,
    ) -> Result<TgMessage, TelegramError> {
        let body = SendMessageBody {
            chat_id,
            text,
            parse_mode,
        };
        let raw_body =
            serde_json::to_vec(&body).map_err(|e| TelegramError::Network(e.to_string()))?;
        let raw = (self.post)("sendMessage".into(), raw_body).await?;
        interpret_response(&raw)
    }
}

// ---------------------------------------------------------------------------
// Pure core
// ---------------------------------------------------------------------------

/// Parse a raw HTTP response into a Telegram API result.
///
/// Pure function — no IO, no async. Deserializes the `ApiResponse<T>`
/// envelope and unwraps it into a domain result.
pub fn interpret_response<T: DeserializeOwned>(raw: &RawResponse) -> Result<T, TelegramError> {
    let api_response: ApiResponse<T> =
        serde_json::from_slice(&raw.body).map_err(|e| TelegramError::Deserialize(e.to_string()))?;

    if api_response.ok {
        api_response.result.ok_or_else(|| TelegramError::Api {
            error_code: 0,
            description: "ok=true but missing result".into(),
        })
    } else {
        Err(TelegramError::Api {
            error_code: api_response.error_code.unwrap_or(0),
            description: api_response.description.unwrap_or_default(),
        })
    }
}

// ---------------------------------------------------------------------------
// Wire format types (Telegram Bot API)
// ---------------------------------------------------------------------------

// Intentionally no `deny_unknown_fields` — Telegram returns many fields
// we don't care about, and the API grows over time.

/// Wrapper returned by every Bot API method.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ApiResponse<T> {
    pub(crate) ok: bool,
    pub(crate) result: Option<T>,
    pub(crate) error_code: Option<i32>,
    pub(crate) description: Option<String>,
}

/// A single incoming update from `getUpdates`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<TgMessage>,
}

/// A Telegram message.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TgMessage {
    pub message_id: i64,
    pub chat: Chat,
    pub text: Option<String>,
}

/// Identifies the conversation a message belongs to.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Chat {
    pub id: i64,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct GetUpdatesBody {
    pub(crate) offset: i64,
    pub(crate) timeout: u64,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct SendMessageBody<'a> {
    pub(crate) chat_id: i64,
    pub(crate) text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parse_mode: Option<&'a str>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(body: &str) -> RawResponse {
        RawResponse {
            status: 200,
            body: body.as_bytes().to_vec(),
        }
    }

    fn ok_updates_json(updates: &[Update]) -> String {
        serde_json::to_string(&ApiResponse {
            ok: true,
            result: Some(updates),
            error_code: None,
            description: None,
        })
        .unwrap()
    }

    fn ok_send_json() -> String {
        serde_json::to_string(&ApiResponse {
            ok: true,
            result: Some(TgMessage {
                message_id: 1,
                chat: Chat { id: 42 },
                text: None,
            }),
            error_code: None,
            description: None,
        })
        .unwrap()
    }

    fn api_error_json(code: i32, desc: &str) -> String {
        serde_json::to_string(&ApiResponse::<serde_json::Value> {
            ok: false,
            result: None,
            error_code: Some(code),
            description: Some(desc.into()),
        })
        .unwrap()
    }

    // -- interpret_response tests --

    #[test]
    fn interpret_poll_updates_success() {
        let json = ok_updates_json(&[Update {
            update_id: 7,
            message: Some(TgMessage {
                message_id: 1,
                chat: Chat { id: 42 },
                text: Some("hello".into()),
            }),
        }]);
        let updates: Vec<Update> = interpret_response(&raw(&json)).unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].update_id, 7);
        assert_eq!(
            updates[0].message.as_ref().unwrap().text.as_deref(),
            Some("hello"),
        );
    }

    #[test]
    fn interpret_poll_updates_empty() {
        let json = ok_updates_json(&[]);
        let updates: Vec<Update> = interpret_response(&raw(&json)).unwrap();
        assert!(updates.is_empty());
    }

    #[test]
    fn interpret_api_error() {
        let json = api_error_json(401, "Unauthorized");
        let err = interpret_response::<Vec<Update>>(&raw(&json)).unwrap_err();
        assert!(matches!(
            err,
            TelegramError::Api {
                error_code: 401,
                ..
            }
        ));
    }

    #[test]
    fn interpret_malformed_json() {
        let err = interpret_response::<Vec<Update>>(&raw("not json")).unwrap_err();
        assert!(matches!(err, TelegramError::Deserialize(_)));
    }

    #[test]
    fn interpret_post_message_success() {
        let json = ok_send_json();
        let msg: TgMessage = interpret_response(&raw(&json)).unwrap();
        assert_eq!(msg.message_id, 1);
    }

    #[test]
    fn interpret_post_message_api_error() {
        let json = api_error_json(400, "Bad Request");
        let err = interpret_response::<TgMessage>(&raw(&json)).unwrap_err();
        assert!(matches!(
            err,
            TelegramError::Api {
                error_code: 400,
                ..
            }
        ));
    }

    // -- Client integration tests via from_fn --

    #[tokio::test]
    async fn client_poll_updates_roundtrip() {
        let json = ok_updates_json(&[Update {
            update_id: 1,
            message: None,
        }]);
        let client = TelegramClient::from_fn(move |_method, _body| {
            let json = json.clone();
            async move {
                Ok(RawResponse {
                    status: 200,
                    body: json.into_bytes(),
                })
            }
        });

        let updates = client.poll_updates(0, 30).await.unwrap();
        assert_eq!(updates.len(), 1);
    }

    #[tokio::test]
    async fn client_propagates_closure_error() {
        let client = TelegramClient::from_fn(|_method, _body| async {
            Err(TelegramError::Network("boom".into()))
        });

        let err = client.poll_updates(0, 30).await.unwrap_err();
        assert!(matches!(err, TelegramError::Network(_)));
    }
}
