//! OpenAI-compatible provider implementation.
//!
//! Bridges the [`Provider`] trait with any endpoint that speaks the
//! `OpenAI` chat completions wire format.

use std::time::Duration;

use serde::Serialize;
use tracing::{debug, trace, warn};

use crate::clients::chat_completion::{ApiToolCall, ChatResponse, CompletionsClient};
use crate::config::{Api, ProviderConfig};
use crate::error::ProviderError;
use crate::types::{Message, Response, ToolCall, ToolDefinition, ToolFunction};

use super::wire::WireMessage;

use super::{ChatOutcome, Provider};

/// Retries after the initial attempt for transient failures.
const MAX_RETRIES: u32 = 3;

/// Retry policy for chat requests: exponential backoff for transient
/// errors, starting at 1s, or 5s when rate limited (the window is
/// typically seconds, not milliseconds). No jitter: one daemon, no
/// thundering herd.
fn retry_policy(e: &ProviderError, attempt: u32) -> Option<Duration> {
    if !e.is_transient() || attempt >= MAX_RETRIES {
        return None;
    }
    let base = if matches!(e, ProviderError::RateLimited) {
        5
    } else {
        1
    };
    Some(Duration::from_secs(base << attempt))
}

/// Provider for any OpenAI-compatible chat completions endpoint.
pub struct CompletionsProvider {
    client: CompletionsClient,
    model: String,
    max_tokens: u32,
    temperature: f32,
    /// Request `OpenRouter` usage accounting (cache hit details).
    /// Off for other APIs — strict endpoints reject unknown params.
    usage_accounting: bool,
}

impl CompletionsProvider {
    /// Create a new provider with the given client and configuration.
    pub fn new(client: CompletionsClient, config: &ProviderConfig) -> Self {
        Self {
            client,
            model: config.model.clone(),
            max_tokens: config.max_tokens,
            temperature: config.temperature,
            usage_accounting: matches!(config.api, Api::OpenRouter),
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
            usage_accounting: self.usage_accounting,
        }
    }

    /// Parse the API response into our domain type.
    fn parse_response(response: ChatResponse) -> Result<Response, ProviderError> {
        let choice =
            response.choices.into_iter().next().ok_or_else(|| {
                ProviderError::InvalidResponse("no choices in response".to_string())
            })?;

        let content = choice.message.content.unwrap_or_default();
        // `length` means generation was cut off by max_tokens —
        // typically a reasoning model spending the whole budget
        // on reasoning.
        let truncated = choice.finish_reason.as_deref() == Some("length");

        match choice.message.tool_calls {
            Some(calls) if !calls.is_empty() => {
                let calls = calls.into_iter().map(into_tool_call).collect();
                Ok(Response::ToolCalls { content, calls })
            }
            // A text response with nothing in it is a provider fault,
            // not a reply. Truncation is distinct from a genuinely
            // empty reply.
            _ if content.trim().is_empty() => {
                warn!(
                    finish_reason = choice.finish_reason.as_deref().unwrap_or("<missing>"),
                    reasoning_len = choice.message.reasoning.as_ref().map_or(0, String::len),
                    "Provider returned empty content with no tool calls"
                );
                if truncated {
                    Err(ProviderError::Truncated)
                } else {
                    Err(ProviderError::EmptyResponse)
                }
            }
            // Partial text is still worth surfacing, but mark the cut
            // so readers (and the model, next turn) know the reply is
            // incomplete.
            _ if truncated => {
                warn!(
                    content_len = content.len(),
                    "Provider response truncated at max_tokens"
                );
                Ok(Response::Text(format!(
                    "{content}\n\n[truncated at max_tokens]"
                )))
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
    ) -> Result<ChatOutcome, ProviderError> {
        let wire_messages: Vec<WireMessage> = messages.iter().map(WireMessage::from).collect();
        let request = ChatRequest {
            model: &self.model,
            messages: wire_messages,
            tools: if tools.is_empty() { None } else { Some(tools) },
            max_tokens: self.max_tokens,
            temperature: self.temperature,
            usage: self
                .usage_accounting
                .then_some(UsageAccounting { include: true }),
        };

        debug!(model = %self.model, message_count = messages.len(), "Sending chat request");
        trace!(request = %serde_json::to_string(&request).unwrap_or_default(), "Request body");

        let response =
            crate::retry::retry(|| self.client.chat_completions(&request), retry_policy).await?;
        if let Some(usage) = &response.usage {
            debug!(
                model = %self.model,
                prompt_tokens = usage.prompt_tokens,
                cached_tokens = usage
                    .prompt_tokens_details
                    .as_ref()
                    .map_or(0, |d| d.cached_tokens),
                completion_tokens = usage.completion_tokens,
                "Usage"
            );
        }
        let prompt_tokens = response.usage.as_ref().map(|u| u.prompt_tokens);
        Ok(ChatOutcome {
            response: Self::parse_response(response)?,
            prompt_tokens,
        })
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
    /// `OpenRouter` usage accounting opt-in; omitted for other APIs.
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<UsageAccounting>,
}

#[derive(Serialize)]
struct UsageAccounting {
    include: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::chat_completion::{ApiFunction, AssistantMessage, Choice};

    fn response(msg: AssistantMessage) -> ChatResponse {
        response_with_finish(msg, None)
    }

    fn response_with_finish(msg: AssistantMessage, finish_reason: Option<&str>) -> ChatResponse {
        ChatResponse {
            choices: vec![Choice {
                message: msg,
                finish_reason: finish_reason.map(str::to_string),
            }],
            citations: Vec::new(),
            usage: None,
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
        assert_eq!(cheap.usage_accounting, provider.usage_accounting);
    }

    #[test]
    fn usage_accounting_serialized_only_when_set() {
        let request = |usage| ChatRequest {
            model: "m",
            messages: Vec::new(),
            tools: None,
            max_tokens: 1,
            temperature: 0.0,
            usage,
        };
        let with =
            serde_json::to_string(&request(Some(UsageAccounting { include: true }))).unwrap();
        assert!(with.contains(r#""usage":{"include":true}"#));
        let without = serde_json::to_string(&request(None)).unwrap();
        assert!(!without.contains("usage"));
    }

    #[test]
    fn usage_accounting_enabled_only_for_openrouter() {
        let client = || {
            CompletionsClient::new(
                "https://example.invalid".to_string(),
                crate::secrets::Secret::test("k"),
            )
        };
        // Default API is OpenRouter.
        let config = ProviderConfig::default();
        assert!(CompletionsProvider::new(client(), &config).usage_accounting);
        let config = ProviderConfig {
            api: Api::OpenAi,
            ..ProviderConfig::default()
        };
        assert!(!CompletionsProvider::new(client(), &config).usage_accounting);
    }

    #[tokio::test]
    async fn chat_surfaces_prompt_tokens_from_usage() {
        let client = CompletionsClient::from_fn(|_body| async {
            Ok(crate::clients::RawResponse {
                status: 200,
                body: br#"{
                    "choices":[{"message":{"content":"hi"}}],
                    "usage":{"prompt_tokens":42,"completion_tokens":7}
                }"#
                .to_vec(),
            })
        });
        let provider = CompletionsProvider::new(client, &ProviderConfig::default());
        let outcome = provider.chat(&[], &[]).await.unwrap();
        assert_eq!(outcome.prompt_tokens, Some(42));
    }

    #[tokio::test]
    async fn chat_without_usage_has_no_prompt_tokens() {
        let client = CompletionsClient::from_fn(|_body| async {
            Ok(crate::clients::RawResponse {
                status: 200,
                body: br#"{"choices":[{"message":{"content":"hi"}}]}"#.to_vec(),
            })
        });
        let provider = CompletionsProvider::new(client, &ProviderConfig::default());
        let outcome = provider.chat(&[], &[]).await.unwrap();
        assert_eq!(outcome.prompt_tokens, None);
    }

    #[test]
    fn policy_doubles_from_one_second() {
        let e = ProviderError::Network("reset".into());
        assert_eq!(retry_policy(&e, 0), Some(Duration::from_secs(1)));
        assert_eq!(retry_policy(&e, 1), Some(Duration::from_secs(2)));
        assert_eq!(retry_policy(&e, 2), Some(Duration::from_secs(4)));
    }

    #[test]
    fn policy_rate_limited_doubles_from_five_seconds() {
        let e = ProviderError::RateLimited;
        assert_eq!(retry_policy(&e, 0), Some(Duration::from_secs(5)));
        assert_eq!(retry_policy(&e, 1), Some(Duration::from_secs(10)));
        assert_eq!(retry_policy(&e, 2), Some(Duration::from_secs(20)));
    }

    #[test]
    fn policy_stops_at_max_retries() {
        let e = ProviderError::RateLimited;
        assert_eq!(retry_policy(&e, MAX_RETRIES), None);
    }

    #[test]
    fn policy_rejects_fatal_errors() {
        assert_eq!(
            retry_policy(&ProviderError::BadRequest("400".into()), 0),
            None
        );
        assert_eq!(retry_policy(&ProviderError::Authentication, 0), None);
    }

    /// Provider whose client fails `failures` times before succeeding,
    /// counting every request.
    fn flaky_provider(
        failures: usize,
        error: ProviderError,
    ) -> (
        CompletionsProvider,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let client = CompletionsClient::from_fn(move |_body| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let error = error.clone();
            async move {
                if n < failures {
                    Err(error)
                } else {
                    Ok(crate::clients::RawResponse {
                        status: 200,
                        body: br#"{"choices":[{"message":{"content":"hi"}}]}"#.to_vec(),
                    })
                }
            }
        });
        (
            CompletionsProvider::new(client, &ProviderConfig::default()),
            calls,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn chat_retries_transient_errors_until_success() {
        use std::sync::atomic::Ordering;
        let (provider, calls) = flaky_provider(2, ProviderError::ServerError("503".into()));
        let outcome = provider.chat(&[], &[]).await.unwrap();
        assert!(matches!(outcome.response, Response::Text(t) if t == "hi"));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn chat_does_not_retry_fatal_errors() {
        use std::sync::atomic::Ordering;
        let (provider, calls) = flaky_provider(usize::MAX, ProviderError::BadRequest("400".into()));
        let err = provider.chat(&[], &[]).await.unwrap_err();
        assert!(matches!(err, ProviderError::BadRequest(_)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn chat_gives_up_after_max_retries() {
        use std::sync::atomic::Ordering;
        let (provider, calls) = flaky_provider(usize::MAX, ProviderError::Network("reset".into()));
        let err = provider.chat(&[], &[]).await.unwrap_err();
        assert!(matches!(err, ProviderError::Network(_)));
        assert_eq!(calls.load(Ordering::SeqCst), 1 + MAX_RETRIES as usize);
    }

    #[tokio::test(start_paused = true)]
    async fn chat_rate_limit_waits_longer() {
        let start = tokio::time::Instant::now();
        let (provider, _) = flaky_provider(usize::MAX, ProviderError::RateLimited);
        provider.chat(&[], &[]).await.unwrap_err();
        // 5s + 10s + 20s across the three retries.
        assert_eq!(start.elapsed(), Duration::from_secs(35));
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
    fn empty_content_with_length_finish_is_truncated_error() {
        let result = CompletionsProvider::parse_response(response_with_finish(
            AssistantMessage {
                content: None,
                tool_calls: None,
                reasoning: Some("thinking...".to_string()),
            },
            Some("length"),
        ));
        assert!(matches!(result, Err(ProviderError::Truncated)));
    }

    #[test]
    fn empty_content_with_stop_finish_is_empty_response_error() {
        let result = CompletionsProvider::parse_response(response_with_finish(
            AssistantMessage {
                content: None,
                tool_calls: None,
                reasoning: None,
            },
            Some("stop"),
        ));
        assert!(matches!(result, Err(ProviderError::EmptyResponse)));
    }

    #[test]
    fn text_with_length_finish_is_surfaced_with_marker() {
        let result = CompletionsProvider::parse_response(response_with_finish(
            AssistantMessage {
                content: Some("partial answer".to_string()),
                tool_calls: None,
                reasoning: None,
            },
            Some("length"),
        ));
        assert!(
            matches!(result, Ok(Response::Text(t)) if t == "partial answer\n\n[truncated at max_tokens]")
        );
    }

    #[test]
    fn text_with_stop_finish_has_no_marker() {
        let result = CompletionsProvider::parse_response(response_with_finish(
            AssistantMessage {
                content: Some("full answer".to_string()),
                tool_calls: None,
                reasoning: None,
            },
            Some("stop"),
        ));
        assert!(matches!(result, Ok(Response::Text(t)) if t == "full answer"));
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
