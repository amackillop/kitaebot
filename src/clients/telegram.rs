//! Telegram Bot API client.
//!
//! HTTP calls go through [`TelegramApi`], responses through [`TelegramClient`].

use std::time::Duration;

use futures::TryFutureExt as _;
use reqwest::{Client, Response};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::TelegramError;
use crate::secrets::Secret;

// ---------------------------------------------------------------------------
// Default client type alias
// ---------------------------------------------------------------------------

/// Concrete client implementation selected by feature flag.
#[cfg(not(feature = "mock-network"))]
pub type TelegramClient = TelegramClientImpl<RealTelegramApi>;

#[cfg(feature = "mock-network")]
pub type TelegramClient = TelegramClientImpl<MockNetworkApi>;

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
    async fn poll_updates(&self, body: GetUpdatesBody) -> Result<Response, reqwest::Error>;

    /// Send a single message via `sendMessage`.
    async fn post_message(&self, body: SendMessageBody<'_>) -> Result<Response, reqwest::Error>;
}

const BASE_URL: &str = "https://api.telegram.org/bot";

pub struct RealTelegramApi {
    bot_token: Secret,
    client: Client,
}

impl RealTelegramApi {
    #[cfg_attr(feature = "mock-network", allow(dead_code))]
    pub fn new(bot_token: Secret, timeout: Duration) -> Self {
        Self {
            bot_token,
            client: Client::builder()
                .timeout(timeout)
                .build()
                .expect("failed to build HTTP client"),
        }
    }

    fn url(&self, method: &str) -> String {
        format!("{BASE_URL}{}/{method}", self.bot_token.expose())
    }
}

impl TelegramApi for RealTelegramApi {
    async fn poll_updates(&self, body: GetUpdatesBody) -> Result<Response, reqwest::Error> {
        self.client
            .post(self.url("getUpdates"))
            .json(&body)
            .send()
            .await
    }

    async fn post_message(&self, body: SendMessageBody<'_>) -> Result<Response, reqwest::Error> {
        self.client
            .post(self.url("sendMessage"))
            .json(&body)
            .send()
            .await
    }
}

// ---------------------------------------------------------------------------
// Stub API client (mock-network builds)
// ---------------------------------------------------------------------------

#[cfg(feature = "mock-network")]
#[derive(Clone)]
#[allow(dead_code)]
pub struct MockNetworkApi;

#[cfg(feature = "mock-network")]
impl TelegramApi for MockNetworkApi {
    async fn poll_updates(&self, _body: GetUpdatesBody) -> Result<Response, reqwest::Error> {
        let body = r#"{"ok":true,"result":[]}"#;
        Ok(Response::from(
            http::Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(body)
                .unwrap(),
        ))
    }

    async fn post_message(&self, _body: SendMessageBody<'_>) -> Result<Response, reqwest::Error> {
        let body = r#"{"ok":true,"result":{"message_id":1}}"#;
        Ok(Response::from(
            http::Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(body)
                .unwrap(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Generic client (response parsing + error mapping)
// ---------------------------------------------------------------------------

/// HTTP client for the Telegram Bot API.
///
/// Generic over [`TelegramApi`] so that tests can substitute a stub
/// without bypassing response parsing.
pub struct TelegramClientImpl<A> {
    api: A,
}

impl<A: TelegramApi> TelegramClientImpl<A> {
    pub fn new(api: A) -> Self {
        Self { api }
    }

    pub async fn poll_updates(
        &self,
        offset: i64,
        timeout: u64,
    ) -> Result<Vec<Update>, TelegramError> {
        let body = GetUpdatesBody { offset, timeout };
        let resp = self.api.poll_updates(body);

        process_response(resp).await
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
        let resp = self.api.post_message(body);

        process_response(resp).await
    }
}

async fn process_response<T: DeserializeOwned>(
    res: impl Future<Output = Result<Response, reqwest::Error>>,
) -> Result<T, TelegramError> {
    let api_response: ApiResponse<T> = res
        .map_err(|e| TelegramError::Network(e.to_string()))
        .await?
        .json()
        .await
        .map_err(|e| TelegramError::Deserialize(e.to_string()))?;

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
#[cfg_attr(feature = "mock-network", allow(dead_code))]
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    /// Stub [`TelegramApi`] that yields pre-configured HTTP responses.
    ///
    /// Both trait methods pop from the same queue, so tests enqueue
    /// exactly the responses they expect in call order.
    struct StubApi(Mutex<VecDeque<Result<Response, reqwest::Error>>>);

    impl StubApi {
        fn client(responses: Vec<Result<Response, reqwest::Error>>) -> TelegramClientImpl<Self> {
            TelegramClientImpl::new(Self(Mutex::new(responses.into())))
        }
    }

    impl TelegramApi for StubApi {
        async fn poll_updates(&self, _body: GetUpdatesBody) -> Result<Response, reqwest::Error> {
            self.0.lock().unwrap().pop_front().unwrap()
        }

        async fn post_message(
            &self,
            _body: SendMessageBody<'_>,
        ) -> Result<Response, reqwest::Error> {
            self.0.lock().unwrap().pop_front().unwrap()
        }
    }

    fn json_response(body: &impl Serialize) -> Response {
        let json = serde_json::to_string(body).unwrap();
        Response::from(
            http::Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(json)
                .unwrap(),
        )
    }

    fn reqwest_error() -> reqwest::Error {
        Response::from(http::Response::builder().status(500).body("").unwrap())
            .error_for_status()
            .unwrap_err()
    }

    fn ok_updates(updates: Vec<Update>) -> Response {
        json_response(&ApiResponse {
            ok: true,
            result: Some(updates),
            error_code: None,
            description: None,
        })
    }

    fn ok_send() -> Response {
        json_response(&ApiResponse {
            ok: true,
            result: Some(TgMessage {
                message_id: 1,
                chat: Chat { id: 42 },
                text: None,
            }),
            error_code: None,
            description: None,
        })
    }

    fn api_error(code: i32, desc: &str) -> Response {
        json_response(&ApiResponse::<serde_json::Value> {
            ok: false,
            result: None,
            error_code: Some(code),
            description: Some(desc.into()),
        })
    }

    #[tokio::test]
    async fn client_poll_updates_parses_response() {
        let client = StubApi::client(vec![Ok(ok_updates(vec![Update {
            update_id: 7,
            message: Some(TgMessage {
                message_id: 1,
                chat: Chat { id: 42 },
                text: Some("hello".into()),
            }),
        }]))]);

        let updates = client.poll_updates(0, 30).await.unwrap();

        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].update_id, 7);
        assert_eq!(
            updates[0].message.as_ref().unwrap().text.as_deref(),
            Some("hello"),
        );
    }

    #[tokio::test]
    async fn client_poll_updates_empty() {
        let client = StubApi::client(vec![Ok(ok_updates(vec![]))]);

        let updates = client.poll_updates(0, 30).await.unwrap();

        assert!(updates.is_empty());
    }

    #[tokio::test]
    async fn client_poll_updates_api_error() {
        let client = StubApi::client(vec![Ok(api_error(401, "Unauthorized"))]);

        let err = client.poll_updates(0, 30).await.unwrap_err();

        assert!(matches!(
            err,
            TelegramError::Api {
                error_code: 401,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn client_poll_updates_network_error() {
        let client = StubApi::client(vec![Err(reqwest_error())]);

        let err = client.poll_updates(0, 30).await.unwrap_err();

        assert!(matches!(err, TelegramError::Network(_)));
    }

    #[tokio::test]
    async fn client_poll_updates_malformed_json() {
        let garbage = Response::from(
            http::Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body("not json")
                .unwrap(),
        );
        let client = StubApi::client(vec![Ok(garbage)]);

        let err = client.poll_updates(0, 30).await.unwrap_err();

        assert!(matches!(err, TelegramError::Deserialize(_)));
    }

    #[tokio::test]
    async fn client_post_message_success() {
        let client = StubApi::client(vec![Ok(ok_send())]);

        client.post_message(42, "hi", None).await.unwrap();
    }

    #[tokio::test]
    async fn client_post_message_api_error() {
        let client = StubApi::client(vec![Ok(api_error(400, "Bad Request"))]);

        let err = client.post_message(42, "hi", None).await.unwrap_err();

        assert!(matches!(
            err,
            TelegramError::Api {
                error_code: 400,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn client_post_message_network_error() {
        let client = StubApi::client(vec![Err(reqwest_error())]);

        let err = client.post_message(42, "hi", None).await.unwrap_err();

        assert!(matches!(err, TelegramError::Network(_)));
    }
}
