//! `OpenRouter` provider implementation.
//!
//! Communicates with `OpenRouter`'s OpenAI-compatible API to get LLM responses.

use std::env;

use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::error::ProviderError;
use crate::types::{Message, Response, ToolCall, ToolDefinition, ToolFunction};

use super::Provider;

/// Configuration for the `OpenRouter` provider.
pub struct OpenRouterConfig {
    /// Model identifier (e.g., "anthropic/claude-sonnet-4").
    pub model: String,
    /// Maximum tokens in response.
    pub max_tokens: u32,
    /// Sampling temperature.
    pub temperature: f32,
}

impl Default for OpenRouterConfig {
    fn default() -> Self {
        Self {
            model: "anthropic/claude-sonnet-4".to_string(),
            max_tokens: 4096,
            temperature: 0.7,
        }
    }
}

/// `OpenRouter` LLM provider.
///
/// Makes HTTP requests to `OpenRouter`'s chat completions endpoint.
pub struct OpenRouterProvider {
    client: Client,
    api_key: String,
    config: OpenRouterConfig,
}

impl OpenRouterProvider {
    const ENDPOINT: &'static str = "https://openrouter.ai/api/v1/chat/completions";

    /// Create a new provider with the given API key and configuration.
    pub fn new(api_key: String, config: OpenRouterConfig) -> Self {
        Self {
            client: Client::new(),
            api_key,
            config,
        }
    }

    /// Create a provider using the `OPENROUTER_API_KEY` environment variable.
    ///
    /// # Errors
    ///
    /// Returns `ProviderError::Authentication` if the environment variable is not set.
    pub fn from_env() -> Result<Self, ProviderError> {
        let api_key = env::var("OPENROUTER_API_KEY").map_err(|_| ProviderError::Authentication)?;
        Ok(Self::new(api_key, OpenRouterConfig::default()))
    }

    /// Parse the API response into our domain type.
    fn parse_response(response: ChatResponse) -> Result<Response, ProviderError> {
        let choice =
            response.choices.into_iter().next().ok_or_else(|| {
                ProviderError::InvalidResponse("no choices in response".to_string())
            })?;

        match choice.message.tool_calls {
            Some(calls) if !calls.is_empty() => {
                let tool_calls = calls
                    .into_iter()
                    .map(|tc| ToolCall {
                        id: tc.id,
                        function: ToolFunction {
                            name: tc.function.name,
                            arguments: tc.function.arguments,
                        },
                    })
                    .collect();
                Ok(Response::ToolCalls(tool_calls))
            }
            _ => {
                let content = choice.message.content.unwrap_or_default();
                Ok(Response::Text(content))
            }
        }
    }
}

impl Provider for OpenRouterProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<Response, ProviderError> {
        let request = ChatRequest {
            model: &self.config.model,
            messages,
            tools: if tools.is_empty() { None } else { Some(tools) },
            max_tokens: self.config.max_tokens,
            temperature: self.config.temperature,
        };

        let response = self
            .client
            .post(Self::ENDPOINT)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("HTTP-Referer", "https://github.com/amackillop/kitaebot")
            .header("X-Title", "kitaebot")
            .json(&request)
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        match response.status() {
            status if status.is_success() => {
                let chat_response: ChatResponse = response
                    .json()
                    .await
                    .map_err(|e| ProviderError::InvalidResponse(e.to_string()))?;
                Self::parse_response(chat_response)
            }
            reqwest::StatusCode::UNAUTHORIZED => Err(ProviderError::Authentication),
            reqwest::StatusCode::TOO_MANY_REQUESTS => Err(ProviderError::RateLimited),
            status => {
                let body = response.text().await.unwrap_or_default();
                Err(ProviderError::Network(format!("{status}: {body}")))
            }
        }
    }
}

// --- Wire format types (private) ---

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [ToolDefinition]>,
    max_tokens: u32,
    temperature: f32,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: AssistantMessage,
}

#[derive(Deserialize)]
struct AssistantMessage {
    content: Option<String>,
    tool_calls: Option<Vec<ApiToolCall>>,
}

#[derive(Deserialize)]
struct ApiToolCall {
    id: String,
    function: ApiFunction,
}

#[derive(Deserialize)]
struct ApiFunction {
    name: String,
    arguments: String,
}
