//! OpenAI-compatible provider implementation.
//!
//! Bridges the [`Provider`] trait with any endpoint that speaks the
//! `OpenAI` chat completions wire format.

use serde::Serialize;
use tracing::{debug, trace, warn};

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

    /// A variant of this provider using a different model. Client,
    /// `max_tokens`, and temperature are shared.
    pub fn with_model(&self, model: &str) -> Self {
        Self {
            client: self.client.clone(),
            model: model.to_string(),
            max_tokens: self.max_tokens,
            temperature: self.temperature,
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
            // A text response with nothing in it is a provider fault,
            // not a reply.
            _ if content.trim().is_empty() => {
                warn!(
                    reasoning_len = choice.message.reasoning.as_ref().map_or(0, String::len),
                    "Provider returned empty content with no tool calls"
                );
                Err(ProviderError::EmptyResponse)
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
    messages: Vec<WireMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [ToolDefinition]>,
    max_tokens: u32,
    temperature: f32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::chat_completion::{ApiFunction, AssistantMessage, Choice};

    fn response(msg: AssistantMessage) -> ChatResponse {
        ChatResponse {
            choices: vec![Choice { message: msg }],
            citations: Vec::new(),
        }
    }

    fn tool_call() -> ApiToolCall {
        ApiToolCall {
            id: "call-1".to_string(),
            function: ApiFunction {
                name: "exec".to_string(),
                arguments: "{}".to_string(),
            },
        }
    }

    #[test]
    fn with_model_swaps_model_and_keeps_the_rest() {
        let client = CompletionsClient::new(
            "https://example.invalid".to_string(),
            crate::secrets::Secret::test("k"),
        );
        let provider = CompletionsProvider::new(client, &ProviderConfig::default());
        let cheap = provider.with_model("cheap/model");
        assert_eq!(cheap.model, "cheap/model");
        assert_eq!(cheap.max_tokens, provider.max_tokens);
        assert!((cheap.temperature - provider.temperature).abs() < f32::EPSILON);
    }

    #[test]
    fn text_response_parses() {
        let result = CompletionsProvider::parse_response(response(AssistantMessage {
            content: Some("hello".to_string()),
            tool_calls: None,
            reasoning: None,
        }));
        assert!(matches!(result, Ok(Response::Text(t)) if t == "hello"));
    }

    #[test]
    fn empty_text_is_empty_response_error() {
        let result = CompletionsProvider::parse_response(response(AssistantMessage {
            content: Some(String::new()),
            tool_calls: None,
            reasoning: None,
        }));
        assert!(matches!(result, Err(ProviderError::EmptyResponse)));
    }

    #[test]
    fn whitespace_only_text_is_empty_response_error() {
        let result = CompletionsProvider::parse_response(response(AssistantMessage {
            content: Some("  \n".to_string()),
            tool_calls: None,
            reasoning: None,
        }));
        assert!(matches!(result, Err(ProviderError::EmptyResponse)));
    }

    #[test]
    fn reasoning_without_content_is_empty_response_error() {
        let result = CompletionsProvider::parse_response(response(AssistantMessage {
            content: None,
            tool_calls: None,
            reasoning: Some("thinking...".to_string()),
        }));
        assert!(matches!(result, Err(ProviderError::EmptyResponse)));
    }

    #[test]
    fn empty_content_with_tool_calls_is_valid() {
        let result = CompletionsProvider::parse_response(response(AssistantMessage {
            content: None,
            tool_calls: Some(vec![tool_call()]),
            reasoning: None,
        }));
        assert!(matches!(result, Ok(Response::ToolCalls { .. })));
    }

    #[test]
    fn empty_tool_call_list_with_text_is_text() {
        let result = CompletionsProvider::parse_response(response(AssistantMessage {
            content: Some("done".to_string()),
            tool_calls: Some(Vec::new()),
            reasoning: None,
        }));
        assert!(matches!(result, Ok(Response::Text(t)) if t == "done"));
    }
}
