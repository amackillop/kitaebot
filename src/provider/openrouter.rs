//! `OpenRouter` provider implementation.
//!
//! Communicates with `OpenRouter`'s OpenAI-compatible API to get LLM responses.

use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, trace};

use crate::config::ProviderConfig;
use crate::error::ProviderError;
use crate::secrets::Secret;
use crate::types::{Message, Response, ToolCall, ToolDefinition, ToolFunction};

use super::Provider;

/// `OpenRouter` LLM provider.
///
/// Makes HTTP requests to `OpenRouter`'s chat completions endpoint.
pub struct OpenRouterProvider {
    client: Client,
    api_key: Secret,
    model: String,
    max_tokens: u32,
    temperature: f32,
}

impl OpenRouterProvider {
    const ENDPOINT: &'static str = "https://openrouter.ai/api/v1/chat/completions";

    /// Create a new provider with the given API key and configuration.
    pub fn new(api_key: Secret, config: &ProviderConfig) -> Self {
        Self {
            client: Client::new(),
            api_key,
            model: config.model.clone(),
            max_tokens: config.max_tokens,
            temperature: config.temperature,
        }
    }

    /// Parse the API response into our domain type.
    fn parse_response(response: ChatResponse) -> Result<Response, ProviderError> {
        let choice =
            response.choices.into_iter().next().ok_or_else(|| {
                ProviderError::InvalidResponse("no choices in response".to_string())
            })?;

        let content = choice.message.content.unwrap_or_default();

        match choice.message.tool_calls {
            Some(calls) if !calls.is_empty() => {
                let calls = calls
                    .into_iter()
                    .map(|tc| {
                        ToolCall::new(
                            tc.id,
                            ToolFunction {
                                name: tc.function.name,
                                arguments: tc.function.arguments,
                            },
                        )
                    })
                    .collect();
                Ok(Response::ToolCalls { content, calls })
            }
            _ => Ok(Response::Text(content)),
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
            model: &self.model,
            messages,
            tools: if tools.is_empty() { None } else { Some(tools) },
            max_tokens: self.max_tokens,
            temperature: self.temperature,
        };

        debug!(model = %self.model, message_count = messages.len(), "Sending chat request");
        trace!(request = %serde_json::to_string(&request).unwrap_or_default(), "Request body");

        let response = self
            .client
            .post(Self::ENDPOINT)
            .header("Authorization", format!("Bearer {}", self.api_key.expose()))
            .header("HTTP-Referer", "https://github.com/amackillop/kitaebot")
            .header("X-Title", "kitaebot")
            .json(&request)
            .send()
            .await
            .map_err(|e| {
                error!("Network error: {e}");
                ProviderError::Network(e.to_string())
            })?;

        let status = response.status();
        debug!(%status, "Received response");

        match status {
            s if s.is_success() => {
                let chat_response: ChatResponse = response
                    .json()
                    .await
                    .map_err(|e| ProviderError::InvalidResponse(e.to_string()))?;
                Self::parse_response(chat_response)
            }
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
