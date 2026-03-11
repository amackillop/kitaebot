//! OpenAI-compatible chat completions client.
//!
//! HTTP calls go through [`CompletionsApi`], responses through
//! [`CompletionsClient`]. Works with any OpenAI-compatible endpoint
//! (`OpenRouter`, Groq, Together, Mistral, etc.).

use reqwest::Response;
use serde::{Deserialize, Serialize};
use tracing::{debug, error};

use crate::error::ProviderError;

// ---------------------------------------------------------------------------
// Default API type alias
// ---------------------------------------------------------------------------

/// Concrete API implementation selected by feature flag.
#[cfg(not(feature = "mock-network"))]
pub type CompletionsClient = CompletionsClientImpl<RealCompletionsApi>;

#[cfg(feature = "mock-network")]
pub type CompletionsClient = CompletionsClientImpl<MockNetworkApi>;

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Abstraction over the chat completions HTTP call.
///
/// Implemented by the real HTTP client, the `mock-network` stub, and the
/// test mock. [`CompletionsClient`] is generic over this trait so that
/// tests can exercise the real response-parsing code without hitting the
/// network.
pub trait CompletionsApi: Send + Sync {
    fn chat_completions<R: Serialize + Send + Sync>(
        &self,
        request: &R,
    ) -> impl std::future::Future<Output = Result<Response, reqwest::Error>> + Send;
}

// ---------------------------------------------------------------------------
// Real API client
// ---------------------------------------------------------------------------

#[cfg(not(feature = "mock-network"))]
use crate::{
    error::SecretError,
    secrets::{self, Secret},
};
#[cfg(not(feature = "mock-network"))]
use reqwest::Client;

/// Raw HTTP client for any OpenAI-compatible chat completions endpoint.
///
/// Owns the `reqwest::Client`, API key, and endpoint URL. Cheap to
/// clone (both `reqwest::Client` and `Secret` are `Arc`-backed).
#[cfg(not(feature = "mock-network"))]
#[derive(Clone)]
pub struct RealCompletionsApi {
    client: Client,
    endpoint: String,
    api_key: Secret,
}

#[cfg(not(feature = "mock-network"))]
impl RealCompletionsApi {
    pub fn new(endpoint: &str) -> Result<Self, SecretError> {
        secrets::load_secret("provider-api-key").map(|api_key| Self {
            client: Client::new(),
            endpoint: endpoint.to_string(),
            api_key,
        })
    }
}

#[cfg(not(feature = "mock-network"))]
impl CompletionsApi for RealCompletionsApi {
    async fn chat_completions<R: Serialize + Send + Sync>(
        &self,
        request: &R,
    ) -> Result<Response, reqwest::Error> {
        self.client
            .post(&self.endpoint)
            .header("Authorization", format!("Bearer {}", self.api_key.expose()))
            .header("HTTP-Referer", "https://github.com/amackillop/kitaebot")
            .header("X-Title", "kitaebot")
            .json(request)
            .send()
            .await
    }
}

// ---------------------------------------------------------------------------
// Stub API client (mock-network builds)
// ---------------------------------------------------------------------------

#[cfg(feature = "mock-network")]
#[derive(Clone)]
pub struct MockNetworkApi;

#[cfg(feature = "mock-network")]
impl CompletionsApi for MockNetworkApi {
    async fn chat_completions<R: Serialize + Send + Sync>(
        &self,
        _request: &R,
    ) -> Result<Response, reqwest::Error> {
        let body = r#"{"choices":[{"message":{"content":"This is a stub response. Compile without mock-network for real API calls."}}]}"#;
        Ok(Response::from(
            http::Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(body)
                .unwrap(),
        ))
    }
}

// Generic client (response parsing + error mapping)
// ---------------------------------------------------------------------------

/// HTTP client for any OpenAI-compatible chat completions endpoint.
///
/// Generic over [`CompletionsApi`] so that tests can substitute a stub
/// without bypassing response parsing.
#[derive(Clone)]
pub struct CompletionsClientImpl<A> {
    api: A,
}

impl<A: CompletionsApi> CompletionsClientImpl<A> {
    pub fn new(api: A) -> Self {
        Self { api }
    }

    pub async fn chat_completions<R: Serialize + Send + Sync>(
        &self,
        request: &R,
    ) -> Result<ChatResponse, ProviderError> {
        let response = self.api.chat_completions(request).await.map_err(|e| {
            error!("Network error: {e}");
            ProviderError::Network(e.to_string())
        })?;

        let status = response.status();
        debug!(%status, "Chat completions response");

        match status {
            s if s.is_success() => response
                .json()
                .await
                .map_err(|e| ProviderError::InvalidResponse(e.to_string())),
            reqwest::StatusCode::UNAUTHORIZED => {
                error!("Authentication failed");
                Err(ProviderError::Authentication)
            }
            reqwest::StatusCode::TOO_MANY_REQUESTS => {
                error!("Rate limited");
                Err(ProviderError::RateLimited)
            }
            s => {
                let body = response.text().await.unwrap_or_default();
                error!(%s, "Provider error: {body}");
                Err(ProviderError::Network(format!("{s}: {body}")))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Wire format types (OpenAI-compatible response)
// ---------------------------------------------------------------------------

/// Chat completions response.
///
/// Superset of the `OpenAI` format — includes an optional `citations` field
/// returned by Perplexity models via `OpenRouter`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChatResponse {
    pub choices: Vec<Choice>,
    /// Source URLs returned by Perplexity models. Empty for other models.
    #[serde(default)]
    pub citations: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Choice {
    pub message: AssistantMessage,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AssistantMessage {
    pub content: Option<String>,
    pub tool_calls: Option<Vec<ApiToolCall>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ApiToolCall {
    pub id: String,
    pub function: ApiFunction,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ApiFunction {
    pub name: String,
    pub arguments: String,
}

// ---------------------------------------------------------------------------
// Test mock
// ---------------------------------------------------------------------------

#[cfg(test)]
pub mod mock {
    use std::collections::VecDeque;

    use serde::Serialize;
    use tokio::sync::Mutex;

    use super::*;

    /// Stub [`CompletionsApi`] that yields pre-configured HTTP responses.
    ///
    /// Pops from a queue, so tests enqueue exactly the responses they
    /// expect in call order.
    pub struct StubApi(Mutex<VecDeque<Result<Response, reqwest::Error>>>);

    impl StubApi {
        pub fn client(
            responses: Vec<Result<Response, reqwest::Error>>,
        ) -> CompletionsClientImpl<Self> {
            CompletionsClientImpl::new(Self(Mutex::new(responses.into())))
        }
    }

    impl CompletionsApi for StubApi {
        async fn chat_completions<R: Serialize + Send + Sync>(
            &self,
            _request: &R,
        ) -> Result<Response, reqwest::Error> {
            self.0
                .lock()
                .await
                .pop_front()
                .expect("StubApi response queue exhausted - test called client more times than responses provided")
        }
    }

    // -- Convenience constructors --

    pub fn json_response(body: &impl Serialize) -> Response {
        let json = serde_json::to_string(body).unwrap();
        Response::from(
            http::Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(json)
                .unwrap(),
        )
    }

    /// A `ChatResponse` containing a single text message.
    pub fn text_response(s: &str) -> ChatResponse {
        ChatResponse {
            choices: vec![Choice {
                message: AssistantMessage {
                    content: Some(s.to_string()),
                    tool_calls: None,
                },
            }],
            citations: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::mock::*;
    use super::*;
    use crate::error::ProviderError;

    #[tokio::test]
    async fn client_chat_completions_success() {
        let client = StubApi::client(vec![Ok(json_response(&text_response("hello")))]);

        let resp = client
            .chat_completions(&serde_json::json!({}))
            .await
            .unwrap();

        assert_eq!(resp.choices.len(), 1);
        assert_eq!(resp.choices[0].message.content.as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn client_chat_completions_empty_choices() {
        let empty = ChatResponse {
            choices: vec![],
            citations: Vec::new(),
        };
        let client = StubApi::client(vec![Ok(json_response(&empty))]);

        let resp = client
            .chat_completions(&serde_json::json!({}))
            .await
            .unwrap();

        assert!(resp.choices.is_empty());
    }

    #[tokio::test]
    async fn client_chat_completions_unauthorized() {
        let resp = Response::from(http::Response::builder().status(401).body("").unwrap());
        let client = StubApi::client(vec![Ok(resp)]);

        let err = client
            .chat_completions(&serde_json::json!({}))
            .await
            .unwrap_err();

        assert!(matches!(err, ProviderError::Authentication));
    }

    #[tokio::test]
    async fn client_chat_completions_rate_limited() {
        let resp = Response::from(http::Response::builder().status(429).body("").unwrap());
        let client = StubApi::client(vec![Ok(resp)]);

        let err = client
            .chat_completions(&serde_json::json!({}))
            .await
            .unwrap_err();

        assert!(matches!(err, ProviderError::RateLimited));
    }

    #[tokio::test]
    async fn client_chat_completions_server_error() {
        let resp = Response::from(
            http::Response::builder()
                .status(503)
                .body("Service Unavailable")
                .unwrap(),
        );
        let client = StubApi::client(vec![Ok(resp)]);

        let err = client
            .chat_completions(&serde_json::json!({}))
            .await
            .unwrap_err();

        assert!(matches!(err, ProviderError::Network(_)));
    }

    #[tokio::test]
    async fn client_chat_completions_malformed_json() {
        let resp = Response::from(
            http::Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body("not json")
                .unwrap(),
        );
        let client = StubApi::client(vec![Ok(resp)]);

        let err = client
            .chat_completions(&serde_json::json!({}))
            .await
            .unwrap_err();

        assert!(matches!(err, ProviderError::InvalidResponse(_)));
    }

    #[tokio::test]
    async fn client_chat_completions_network_error() {
        let err = Response::from(http::Response::builder().status(500).body("").unwrap())
            .error_for_status()
            .unwrap_err();
        let client = StubApi::client(vec![Err(err)]);

        let result = client
            .chat_completions(&serde_json::json!({}))
            .await
            .unwrap_err();

        assert!(matches!(result, ProviderError::Network(_)));
    }
}
