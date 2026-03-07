//! `OpenRouter` provider implementation.
//!
//! Communicates with `OpenRouter`'s OpenAI-compatible API to get LLM responses.

use serde::Serialize;
use tracing::{debug, trace};

use crate::config::ProviderConfig;
use crate::error::ProviderError;
use crate::openrouter::{ApiToolCall, ChatResponse, CompletionsClient};
use crate::types::{Message, Response, ToolCall, ToolDefinition, ToolFunction};

use super::Provider;

/// `OpenRouter` LLM provider.
///
/// Generic over the [`CompletionsClient`] so that tests can substitute a
/// mock without bypassing response parsing.
pub struct OpenRouterProvider<C> {
    client: C,
    model: String,
    max_tokens: u32,
    temperature: f32,
}

impl<C: CompletionsClient> OpenRouterProvider<C> {
    /// Create a new provider with the given client and configuration.
    pub fn new(client: C, config: &ProviderConfig) -> Self {
        Self {
            client,
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
                let calls = calls.into_iter().map(into_tool_call).collect();
                Ok(Response::ToolCalls { content, calls })
            }
            _ => Ok(Response::Text(content)),
        }
    }
}

impl<C: CompletionsClient> Provider for OpenRouterProvider<C> {
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

        let response = self.client.chat_completions(&request).await?;
        Self::parse_response(response)
    }
}

fn into_tool_call(tc: ApiToolCall) -> ToolCall {
    ToolCall::new(
        tc.id,
        ToolFunction {
            name: tc.function.name,
            arguments: tc.function.arguments,
        },
    )
}

// --- Wire format (request only — response types are in openrouter.rs) ---

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [ToolDefinition]>,
    max_tokens: u32,
    temperature: f32,
}
