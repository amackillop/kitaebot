//! OpenAI-compatible chat completions client.
//!
//! Pure response parsing lives in [`interpret_response`]. The IO layer is a
//! stored closure inside [`CompletionsClient`] — swap it for tests without
//! traits or generics. The endpoint comes from `provider.api`, so e2e tests
//! point it at a local fixture server; `mock-network` builds refuse
//! non-loopback hosts.

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
        #[cfg(feature = "mock-network")]
        super::assert_loopback(&endpoint);
        let client = super::http_client(reqwest::Client::builder());
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
                        retry_after_secs: None,
                    })
                })
            }),
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
            serde_json::to_vec(request).map_err(|e| ProviderError::BadRequest(e.to_string()))?;
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
        401 | 403 => {
            error!(status = raw.status, "Authentication failed");
            Err(ProviderError::Authentication)
        }
        429 => {
            error!("Rate limited");
            Err(ProviderError::RateLimited)
        }
        500..=599 => {
            let body = String::from_utf8_lossy(&raw.body);
            error!(status = raw.status, "Provider server error: {body}");
            Err(ProviderError::ServerError(format!(
                "{}: {body}",
                raw.status
            )))
        }
        s => {
            let body = String::from_utf8_lossy(&raw.body);
            error!(status = s, "Provider rejected the request: {body}");
            Err(ProviderError::BadRequest(format!("{s}: {body}")))
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
    /// Token accounting. Standard in `OpenAI` responses; `OpenRouter`
    /// adds cache details when usage accounting is requested.
    #[serde(default)]
    pub usage: Option<Usage>,
}

/// Token usage reported by the provider.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    /// Charged cost in USD; only `OpenRouter` reports it, and only
    /// when usage accounting is requested.
    pub cost: Option<f64>,
}

/// Breakdown of prompt tokens; carries prompt-cache hits.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct PromptTokensDetails {
    /// Tokens read from the provider's prompt cache.
    pub cached_tokens: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Choice {
    pub message: AssistantMessage,
    /// Why generation stopped (`"stop"`, `"length"`, `"tool_calls"`, ...).
    /// `"length"` means the response hit `max_tokens` mid-generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AssistantMessage {
    pub content: Option<String>,
    pub tool_calls: Option<Vec<ApiToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
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
            retry_after_secs: None,
        }
    }

    fn text_json(s: &str) -> String {
        serde_json::to_string(&ChatResponse {
            choices: vec![Choice {
                message: AssistantMessage {
                    content: Some(s.to_string()),
                    tool_calls: None,
                    reasoning: None,
                },
                finish_reason: None,
            }],
            citations: Vec::new(),
            usage: None,
        })
        .unwrap()
    }

    #[test]
    fn interpret_captures_reasoning() {
        let body = r#"{"choices":[{"message":{"content":"","reasoning":"chain of thought"}}]}"#;
        let resp = interpret_response(&raw(200, body)).unwrap();
        assert_eq!(
            resp.choices[0].message.reasoning.as_deref(),
            Some("chain of thought")
        );
    }

    #[test]
    fn interpret_success() {
        let resp = interpret_response(&raw(200, &text_json("hello"))).unwrap();
        assert_eq!(resp.choices.len(), 1);
        assert_eq!(resp.choices[0].message.content.as_deref(), Some("hello"));
    }

    #[test]
    fn interpret_parses_usage_with_cache_details() {
        let body = r#"{
            "choices":[{"message":{"content":"hi"}}],
            "usage":{
                "prompt_tokens":100,
                "completion_tokens":5,
                "prompt_tokens_details":{"cached_tokens":80}
            }
        }"#;
        let resp = interpret_response(&raw(200, body)).unwrap();
        let usage = resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.prompt_tokens_details.unwrap().cached_tokens, 80);
    }

    #[test]
    fn interpret_missing_usage_is_none() {
        let resp = interpret_response(&raw(200, &text_json("hello"))).unwrap();
        assert!(resp.usage.is_none());
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
    fn interpret_server_error_is_transient() {
        let err = interpret_response(&raw(503, "Service Unavailable")).unwrap_err();
        assert!(matches!(err, ProviderError::ServerError(_)));
        assert!(err.is_transient());
    }

    #[test]
    fn interpret_bad_request_is_not_transient() {
        let err = interpret_response(&raw(400, "tokens in request more than max")).unwrap_err();
        assert!(matches!(err, ProviderError::BadRequest(_)));
        assert!(!err.is_transient());
    }

    #[test]
    fn interpret_forbidden_as_authentication() {
        let err = interpret_response(&raw(403, "")).unwrap_err();
        assert!(matches!(err, ProviderError::Authentication));
        assert!(!err.is_transient());
    }

    #[test]
    fn rate_limited_is_transient() {
        assert!(ProviderError::RateLimited.is_transient());
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
                retry_after_secs: None,
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
