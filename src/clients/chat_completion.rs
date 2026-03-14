//! OpenAI-compatible chat completions client.
//!
//! Pure response parsing lives in [`interpret_response`]. The IO layer is a
//! stored closure inside [`CompletionsClient`] — swap it for tests or
//! `mock-network` builds without traits or generics.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::{debug, error};

use super::RawResponse;
use crate::error::ProviderError;
use crate::secrets::Secret;

// ---------------------------------------------------------------------------
// Closure type alias
// ---------------------------------------------------------------------------

type PostResult = Result<RawResponse, ProviderError>;
type PostFuture = Pin<Box<dyn Future<Output = PostResult> + Send>>;
type PostFn = Arc<dyn Fn(Vec<u8>) -> PostFuture + Send + Sync>;

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// HTTP client for any OpenAI-compatible chat completions endpoint.
///
/// Concrete struct — no generics. The IO strategy is a closure injected at
/// construction time. `Clone` is free (`Arc`).
#[derive(Clone)]
pub struct CompletionsClient {
    post: PostFn,
}

impl CompletionsClient {
    pub fn new(endpoint: String, api_key: Secret) -> Self {
        #[cfg(not(feature = "mock-network"))]
        {
            let client = reqwest::Client::new();
            Self {
                post: Arc::new(move |body| {
                    let client = client.clone();
                    let endpoint = endpoint.clone();
                    let api_key = api_key.clone();
                    Box::pin(async move {
                        let resp = client
                            .post(&endpoint)
                            .header("Authorization", format!("Bearer {}", api_key.expose()))
                            .header("HTTP-Referer", "https://github.com/amackillop/kitaebot")
                            .header("X-Title", "kitaebot")
                            .header("Content-Type", "application/json")
                            .body(body)
                            .send()
                            .await
                            .map_err(|e| {
                                error!("Network error: {e}");
                                ProviderError::Network(e.to_string())
                            })?;
                        let status = resp.status().as_u16();
                        let bytes = resp.bytes().await.map_err(|e| {
                            error!("Failed to read response body: {e}");
                            ProviderError::Network(e.to_string())
                        })?;
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
            let _ = (endpoint, api_key);
            let body = br#"{"choices":[{"message":{"content":"This is a stub response. Compile without mock-network for real API calls."}}]}"#;
            Self {
                post: Arc::new(move |_| {
                    Box::pin(async move {
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
        F: Fn(Vec<u8>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = PostResult> + Send + 'static,
    {
        Self {
            post: Arc::new(move |body| Box::pin(f(body))),
        }
    }

    /// Send a chat completions request and parse the response.
    pub async fn chat_completions<R: Serialize>(
        &self,
        request: &R,
    ) -> Result<ChatResponse, ProviderError> {
        let body =
            serde_json::to_vec(request).map_err(|e| ProviderError::Network(e.to_string()))?;
        let raw = (self.post)(body).await?;
        interpret_response(&raw)
    }
}

// ---------------------------------------------------------------------------
// Pure core
// ---------------------------------------------------------------------------

/// Parse a raw HTTP response into a [`ChatResponse`].
///
/// Pure function — no IO, no async. All status-code routing and JSON
/// deserialization lives here so tests can call it synchronously.
pub fn interpret_response(raw: &RawResponse) -> Result<ChatResponse, ProviderError> {
    debug!(status = raw.status, "Chat completions response");

    match raw.status {
        200..=299 => serde_json::from_slice(&raw.body)
            .map_err(|e| ProviderError::InvalidResponse(e.to_string())),
        401 => {
            error!("Authentication failed");
            Err(ProviderError::Authentication)
        }
        429 => {
            error!("Rate limited");
            Err(ProviderError::RateLimited)
        }
        s => {
            let body = String::from_utf8_lossy(&raw.body);
            error!(status = s, "Provider error: {body}");
            Err(ProviderError::Network(format!("{s}: {body}")))
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ProviderError;

    fn raw(status: u16, body: &str) -> RawResponse {
        RawResponse {
            status,
            body: body.as_bytes().to_vec(),
        }
    }

    fn text_json(s: &str) -> String {
        serde_json::to_string(&ChatResponse {
            choices: vec![Choice {
                message: AssistantMessage {
                    content: Some(s.to_string()),
                    tool_calls: None,
                },
            }],
            citations: Vec::new(),
        })
        .unwrap()
    }

    #[test]
    fn interpret_success() {
        let resp = interpret_response(&raw(200, &text_json("hello"))).unwrap();
        assert_eq!(resp.choices.len(), 1);
        assert_eq!(resp.choices[0].message.content.as_deref(), Some("hello"));
    }

    #[test]
    fn interpret_empty_choices() {
        let body = r#"{"choices":[]}"#;
        let resp = interpret_response(&raw(200, body)).unwrap();
        assert!(resp.choices.is_empty());
    }

    #[test]
    fn interpret_unauthorized() {
        let err = interpret_response(&raw(401, "")).unwrap_err();
        assert!(matches!(err, ProviderError::Authentication));
    }

    #[test]
    fn interpret_rate_limited() {
        let err = interpret_response(&raw(429, "")).unwrap_err();
        assert!(matches!(err, ProviderError::RateLimited));
    }

    #[test]
    fn interpret_server_error() {
        let err = interpret_response(&raw(503, "Service Unavailable")).unwrap_err();
        assert!(matches!(err, ProviderError::Network(_)));
    }

    #[test]
    fn interpret_malformed_json() {
        let err = interpret_response(&raw(200, "not json")).unwrap_err();
        assert!(matches!(err, ProviderError::InvalidResponse(_)));
    }

    #[tokio::test]
    async fn client_roundtrip_via_from_fn() {
        let client = CompletionsClient::from_fn(|_body| async {
            Ok(RawResponse {
                status: 200,
                body: br#"{"choices":[{"message":{"content":"hi"}}]}"#.to_vec(),
            })
        });

        let resp = client
            .chat_completions(&serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(resp.choices[0].message.content.as_deref(), Some("hi"));
    }

    #[tokio::test]
    async fn client_propagates_closure_error() {
        let client = CompletionsClient::from_fn(|_body| async {
            Err(ProviderError::Network("boom".into()))
        });

        let err = client
            .chat_completions(&serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Network(_)));
    }
}
