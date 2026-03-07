//! Shared `OpenRouter` HTTP client.
//!
//! Thin wrapper around `reqwest` that handles authentication, the chat
//! completions endpoint, and status-code-to-error mapping. Used by both
//! the [`Provider`] implementation and tool-internal API calls (e.g.
//! web search).

#[cfg(not(feature = "mock-network"))]
use reqwest::Client;
use serde::{Deserialize, Serialize};
#[cfg(not(feature = "mock-network"))]
use tracing::{debug, error};

use crate::error::ProviderError;
#[cfg(not(feature = "mock-network"))]
use crate::secrets::Secret;

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Abstraction over the `OpenRouter` chat completions endpoint.
///
/// Implemented by the real HTTP client, the `mock-network` stub, and the
/// test mock. `OpenRouterProvider` is generic over this trait so that
/// tests exercise the real response-parsing code.
pub trait CompletionsClient: Send + Sync {
    async fn chat_completions<R: Serialize + Send + Sync>(
        &self,
        request: &R,
    ) -> Result<ChatResponse, ProviderError>;
}

// ---------------------------------------------------------------------------
// Real client
// ---------------------------------------------------------------------------

/// `OpenRouter` HTTP client.
///
/// Owns the `reqwest::Client` and API key. Cheap to clone (both are
/// `Arc`-backed internally).
#[cfg(not(feature = "mock-network"))]
#[derive(Clone)]
pub struct OpenRouterClient {
    client: Client,
    api_key: Secret,
}

#[cfg(not(feature = "mock-network"))]
impl OpenRouterClient {
    const ENDPOINT: &str = "https://openrouter.ai/api/v1/chat/completions";

    pub fn new(api_key: Secret) -> Self {
        Self {
            client: Client::new(),
            api_key,
        }
    }
}

#[cfg(not(feature = "mock-network"))]
impl CompletionsClient for OpenRouterClient {
    async fn chat_completions<R: Serialize + Send + Sync>(
        &self,
        request: &R,
    ) -> Result<ChatResponse, ProviderError> {
        let response = self
            .client
            .post(Self::ENDPOINT)
            .header("Authorization", format!("Bearer {}", self.api_key.expose()))
            .header("HTTP-Referer", "https://github.com/amackillop/kitaebot")
            .header("X-Title", "kitaebot")
            .json(request)
            .send()
            .await
            .map_err(|e| {
                error!("Network error: {e}");
                ProviderError::Network(e.to_string())
            })?;

        let status = response.status();
        debug!(%status, "OpenRouter response");

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
// Stub client (mock-network builds)
// ---------------------------------------------------------------------------

#[cfg(feature = "mock-network")]
#[derive(Clone)]
pub struct OpenRouterClient;

#[cfg(feature = "mock-network")]
impl CompletionsClient for OpenRouterClient {
    async fn chat_completions<R: Serialize + Send + Sync>(
        &self,
        _request: &R,
    ) -> Result<ChatResponse, ProviderError> {
        Ok(ChatResponse {
            choices: vec![Choice {
                message: AssistantMessage {
                    content: Some(
                        "This is a stub response. \
                         Compile without mock-network for real API calls."
                            .to_string(),
                    ),
                    tool_calls: None,
                },
            }],
            citations: Vec::new(),
        })
    }
}

// ---------------------------------------------------------------------------
// Wire format types (OpenAI-compatible response)
// ---------------------------------------------------------------------------

/// Chat completions response.
///
/// Superset of the `OpenAI` format — includes an optional `citations` field
/// returned by Perplexity models via `OpenRouter`.
#[derive(Clone, Deserialize)]
pub struct ChatResponse {
    pub choices: Vec<Choice>,
    /// Source URLs returned by Perplexity models. Empty for other models.
    #[serde(default)]
    #[cfg_attr(feature = "mock-network", allow(dead_code))]
    pub citations: Vec<String>,
}

#[derive(Clone, Deserialize)]
pub struct Choice {
    pub message: AssistantMessage,
}

#[derive(Clone, Deserialize)]
pub struct AssistantMessage {
    pub content: Option<String>,
    pub tool_calls: Option<Vec<ApiToolCall>>,
}

#[derive(Clone, Deserialize)]
pub struct ApiToolCall {
    pub id: String,
    pub function: ApiFunction,
}

#[derive(Clone, Deserialize)]
pub struct ApiFunction {
    pub name: String,
    pub arguments: String,
}

// ---------------------------------------------------------------------------
// Test mock
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(dead_code)]
pub mod mock {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde::Serialize;

    use super::*;

    /// Mock client that returns pre-configured responses in sequence.
    ///
    /// Uses `Arc` internally — cloning shares state, so you can hand one
    /// clone to `OpenRouterProvider` and keep another to inspect
    /// `call_count()`.
    #[derive(Clone)]
    pub struct MockOpenRouterClient {
        responses: Arc<Vec<Result<ChatResponse, ProviderError>>>,
        call_count: Arc<AtomicUsize>,
    }

    impl MockOpenRouterClient {
        pub fn new(responses: Vec<Result<ChatResponse, ProviderError>>) -> Self {
            Self {
                responses: Arc::new(responses),
                call_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        pub fn call_count(&self) -> usize {
            self.call_count.load(Ordering::SeqCst)
        }
    }

    impl CompletionsClient for MockOpenRouterClient {
        async fn chat_completions<R: Serialize + Send + Sync>(
            &self,
            _request: &R,
        ) -> Result<ChatResponse, ProviderError> {
            let index = self.call_count.fetch_add(1, Ordering::SeqCst);
            self.responses[index].clone()
        }
    }

    // -- Convenience constructors for tests --

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

    /// A `ChatResponse` containing tool calls with the given IDs.
    ///
    /// Each call targets a tool named `"mock"` with arguments `"{}"`.
    pub fn tool_calls_response(ids: &[&str]) -> ChatResponse {
        ChatResponse {
            choices: vec![Choice {
                message: AssistantMessage {
                    content: Some(String::new()),
                    tool_calls: Some(
                        ids.iter()
                            .map(|id| ApiToolCall {
                                id: id.to_string(),
                                function: ApiFunction {
                                    name: "mock".to_string(),
                                    arguments: "{}".to_string(),
                                },
                            })
                            .collect(),
                    ),
                },
            }],
            citations: Vec::new(),
        }
    }
}
