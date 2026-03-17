//! OpenAI-compatible provider implementation.
//!
//! Bridges the [`Provider`] trait with any endpoint that speaks the
//! `OpenAI` chat completions wire format.

use serde::Serialize;
use tracing::{debug, trace};

use crate::clients::chat_completion::{ApiToolCall, ChatResponse, CompletionsClient};
use crate::config::ProviderConfig;
use crate::error::ProviderError;
use crate::types::{Message, Response, ToolCall, ToolDefinition, ToolFunction};

use super::wire::WireMessage;

use super::Provider;

/// Provider for any OpenAI-compatible chat completions endpoint.
pub struct CompletionsProvider {
    client: CompletionsClient,
    model: String,
    max_tokens: u32,
    temperature: f32,
}

impl CompletionsProvider {
    /// Create a new provider with the given client and configuration.
    pub fn new(client: CompletionsClient, config: &ProviderConfig) -> Self {
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

impl Provider for CompletionsProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<Response, ProviderError> {
        let wire_messages: Vec<WireMessage> = messages.iter().map(WireMessage::from).collect();
        let request = ChatRequest {
            model: &self.model,
            messages: wire_messages,
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

// --- Wire format (request only — response types are in chat_completion.rs) ---

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [ToolDefinition]>,
    max_tokens: u32,
    temperature: f32,
}
