//! OpenAI-compatible provider implementation.
//!
//! Bridges the [`Provider`] trait with any endpoint that speaks the
//! `OpenAI` chat completions wire format.

use std::time::Duration;

use serde::Serialize;
use tracing::{debug, trace, warn};

use crate::clients::chat_completion::{ApiToolCall, ChatResponse, CompletionsClient};
use crate::config::{Api, ModelSpec, ProviderConfig, Reasoning};
use crate::error::ProviderError;
use crate::types::{Message, Response, ToolCall, ToolDefinition, ToolFunction};

use super::wire::WireMessage;

use super::{CallUsage, ChatOutcome, Provider};

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
    /// Sampling temperature; `None` leaves the endpoint's default
    /// in place.
    temperature: Option<f32>,
    /// Reasoning budget sent with every request; `None` leaves the
    /// model's default in place.
    reasoning: Option<Reasoning>,
    /// `OpenRouter`-only request extensions: usage accounting (cache
    /// hit details) and the data-collection denial. Off for other
    /// APIs — strict endpoints reject unknown params.
    openrouter: bool,
}

impl CompletionsProvider {
    /// Create a new provider with the given client and configuration.
    pub fn new(client: CompletionsClient, config: &ProviderConfig) -> Self {
        Self {
            client,
            model: config.model.clone(),
            max_tokens: config.max_tokens,
            temperature: config.temperature,
            reasoning: config.reasoning,
            openrouter: matches!(config.api, Api::OpenRouter),
        }
    }

    /// A variant of this provider per a role's override spec: swap
    /// the model, bound reasoning, or both. Everything the spec
    /// leaves unset is shared with this provider.
    pub fn with_spec(&self, spec: &ModelSpec) -> Self {
        Self {
            client: self.client.clone(),
            model: spec.model.as_deref().unwrap_or(&self.model).to_string(),
            max_tokens: self.max_tokens,
            temperature: self.temperature,
            reasoning: spec.reasoning.or(self.reasoning),
            openrouter: self.openrouter,
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
                // Parsing each name is what keeps an untransmittable one
                // out of history. The first failure refuses the whole
                // response: a partial push would leave the surviving
                // calls without results.
                let calls = calls
                    .into_iter()
                    .map(into_tool_call)
                    .collect::<Result<_, _>>()?;
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

    /// One round trip: send, account for usage, parse.
    ///
    /// This is the unit the retry policy operates on. Parsing belongs
    /// inside it because a response can be faulty in ways the request
    /// is not to blame for, and only a fresh draw fixes those.
    async fn attempt(&self, request: &ChatRequest<'_>) -> Result<ChatOutcome, ProviderError> {
        let response = self.client.chat_completions(request).await?;
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
        let usage = response
            .usage
            .as_ref()
            .map_or_else(CallUsage::default, |u| CallUsage {
                prompt_tokens: Some(u.prompt_tokens),
                cached_tokens: u.prompt_tokens_details.as_ref().map(|d| d.cached_tokens),
                completion_tokens: u.completion_tokens,
                cost: u.cost,
                provider: None,
            });
        // The endpoint name rides the response root, not the usage
        // block: a reply can name its endpoint even without usage.
        let usage = CallUsage {
            provider: response.provider.clone(),
            ..usage
        };
        Ok(ChatOutcome {
            response: Self::parse_response(response)?,
            usage,
        })
    }
}

impl Provider for CompletionsProvider {
    async fn chat(
        &self,
        session: &str,
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
            reasoning: self.openrouter.then_some(self.reasoning).flatten(),
            usage: self.openrouter.then_some(UsageAccounting { include: true }),
            provider: self.openrouter.then_some(ProviderPreferences {
                data_collection: "deny",
                require_parameters: true,
            }),
            // Sticky cache routing; OpenRouter-only, like the other
            // extensions — strict endpoints reject unknown params.
            session_id: self.openrouter.then_some(session),
        };

        debug!(model = %self.model, message_count = messages.len(), "Sending chat request");
        trace!(request = %serde_json::to_string(&request).unwrap_or_default(), "Request body");

        crate::retry::retry(|| self.attempt(&request), retry_policy).await
    }

    fn model(&self) -> &str {
        &self.model
    }
}

fn into_tool_call(tc: ApiToolCall) -> Result<ToolCall, ProviderError> {
    let name = tc.function.name.parse().map_err(|_| {
        // Log the arguments too: the mangling is usually argument markup
        // that leaked into the name, and seeing both identifies the
        // provider-side encoder bug that produced it.
        warn!(
            name = %tc.function.name,
            arguments = %tc.function.arguments,
            "Provider returned a malformed tool name"
        );
        ProviderError::MalformedToolCall {
            name: tc.function.name.clone(),
        }
    })?;
    Ok(ToolCall::new(
        tc.id,
        ToolFunction {
            name,
            arguments: tc.function.arguments,
        },
    ))
}

// --- Wire format (request only — response types are in chat_completion.rs) ---

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [ToolDefinition]>,
    max_tokens: u32,
    /// Omitted when unset, leaving the endpoint's default; some
    /// models fix sampling server-side and reject the parameter.
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    /// Reasoning budget (`OpenRouter` `reasoning` param); omitted
    /// when unset or for other APIs.
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Reasoning>,
    /// `OpenRouter` usage accounting opt-in; omitted for other APIs.
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<UsageAccounting>,
    /// `OpenRouter` routing preferences; omitted for other APIs.
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<ProviderPreferences>,
    /// `OpenRouter` sticky cache-routing key; omitted for other APIs.
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
}

#[derive(Serialize)]
struct UsageAccounting {
    include: bool,
}

/// `OpenRouter` provider routing preferences.
#[derive(Serialize)]
struct ProviderPreferences {
    /// "deny" restricts routing to endpoints that neither retain nor
    /// train on prompts. Not configurable: privacy is not a knob.
    data_collection: &'static str,
    /// Route only to endpoints supporting every request parameter.
    /// Without it, `OpenRouter` may land on a provider that rejects
    /// a parameter outright (reasoning, or temperature on models
    /// that fix sampling server-side), turning a routing choice into
    /// a failed reviewer gate.
    require_parameters: bool,
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
            provider: None,
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
    fn with_spec_swaps_model_and_keeps_the_rest() {
        let client = CompletionsClient::new(
            "http://127.0.0.1:0".to_string(),
            crate::secrets::Secret::test("k"),
        );
        let provider = CompletionsProvider::new(client, &ProviderConfig::default());
        let cheap = provider.with_spec(&ModelSpec {
            model: Some("cheap/model".into()),
            reasoning: None,
        });
        assert_eq!(cheap.model, "cheap/model");
        assert_eq!(cheap.max_tokens, provider.max_tokens);
        assert_eq!(cheap.reasoning, provider.reasoning);
        assert_eq!(cheap.temperature, provider.temperature);
        assert_eq!(cheap.openrouter, provider.openrouter);
    }

    #[test]
    fn with_spec_bounds_reasoning_without_model_swap() {
        use crate::config::ReasoningEffort;

        let client = CompletionsClient::new(
            "http://127.0.0.1:0".to_string(),
            crate::secrets::Secret::test("k"),
        );
        let root = CompletionsProvider::new(
            client,
            &ProviderConfig {
                reasoning: Some(Reasoning::Effort(ReasoningEffort::High)),
                ..ProviderConfig::default()
            },
        );
        let bounded = root.with_spec(&ModelSpec {
            model: None,
            reasoning: Some(Reasoning::Effort(ReasoningEffort::Low)),
        });
        // Model inherited, reasoning overridden.
        assert_eq!(bounded.model, root.model);
        assert_eq!(
            bounded.reasoning,
            Some(Reasoning::Effort(ReasoningEffort::Low))
        );
        // A model-only spec inherits the root reasoning untouched.
        let named = root.with_spec(&ModelSpec {
            model: Some("other/model".into()),
            reasoning: None,
        });
        assert_eq!(named.reasoning, root.reasoning);
    }

    #[test]
    fn openrouter_extensions_serialized_only_when_set() {
        let request = |usage, provider, session_id| ChatRequest {
            model: "m",
            messages: Vec::new(),
            tools: None,
            max_tokens: 1,
            temperature: None,
            reasoning: None,
            usage,
            provider,
            session_id,
        };
        let with = serde_json::to_string(&request(
            Some(UsageAccounting { include: true }),
            Some(ProviderPreferences {
                data_collection: "deny",
                require_parameters: true,
            }),
            Some("amackillop/kitaebot"),
        ))
        .unwrap();
        assert!(with.contains(r#""usage":{"include":true}"#));
        assert!(
            with.contains(r#""provider":{"data_collection":"deny","require_parameters":true}"#)
        );
        assert!(with.contains(r#""session_id":"amackillop/kitaebot""#));
        let without = serde_json::to_string(&request(None, None, None)).unwrap();
        assert!(!without.contains("usage"));
        assert!(!without.contains("provider"));
        assert!(!without.contains("session_id"));
    }

    #[test]
    fn temperature_serialized_only_when_set() {
        let request = |temperature| ChatRequest {
            model: "m",
            messages: Vec::new(),
            tools: None,
            max_tokens: 1,
            temperature,
            reasoning: None,
            usage: None,
            provider: None,
            session_id: None,
        };
        let set = serde_json::to_string(&request(Some(0.5))).unwrap();
        assert!(set.contains(r#""temperature":0.5"#));
        let unset = serde_json::to_string(&request(None)).unwrap();
        assert!(!unset.contains("temperature"));
    }

    #[test]
    fn reasoning_serialized_in_openrouter_shape() {
        use crate::config::ReasoningEffort;

        let request = |reasoning| ChatRequest {
            model: "m",
            messages: Vec::new(),
            tools: None,
            max_tokens: 1,
            temperature: None,
            reasoning,
            usage: None,
            provider: None,
            session_id: None,
        };
        let effort =
            serde_json::to_string(&request(Some(Reasoning::Effort(ReasoningEffort::Low)))).unwrap();
        assert!(effort.contains(r#""reasoning":{"effort":"low"}"#));
        let cap = serde_json::to_string(&request(Some(Reasoning::MaxTokens(8192)))).unwrap();
        assert!(cap.contains(r#""reasoning":{"max_tokens":8192}"#));
        let unset = serde_json::to_string(&request(None)).unwrap();
        assert!(!unset.contains("reasoning"));
    }

    #[test]
    fn openrouter_extensions_enabled_only_for_openrouter() {
        let client = || {
            CompletionsClient::new(
                "http://127.0.0.1:0".to_string(),
                crate::secrets::Secret::test("k"),
            )
        };
        // Default API is OpenRouter.
        let config = ProviderConfig::default();
        assert!(CompletionsProvider::new(client(), &config).openrouter);
        let config = ProviderConfig {
            api: Api::OpenAi,
            ..ProviderConfig::default()
        };
        assert!(!CompletionsProvider::new(client(), &config).openrouter);
    }

    /// The privacy denial rides every `OpenRouter` request body; a
    /// regression here silently re-enables training-data collection.
    #[tokio::test]
    async fn openrouter_request_carries_data_collection_deny() {
        let client = CompletionsClient::from_fn(|body| async move {
            let body = String::from_utf8(body).unwrap();
            assert!(
                body.contains(r#""provider":{"data_collection":"deny","require_parameters":true}"#),
                "request body missing the data-collection denial: {body}"
            );
            Ok(crate::clients::RawResponse {
                status: 200,
                body: br#"{"choices":[{"message":{"content":"hi"}}]}"#.to_vec(),
                retry_after_secs: None,
            })
        });
        let provider = CompletionsProvider::new(client, &ProviderConfig::default());
        provider.chat("s", &[], &[]).await.unwrap();
    }

    #[tokio::test]
    async fn chat_surfaces_usage_including_cost() {
        let client = CompletionsClient::from_fn(|_body| async {
            Ok(crate::clients::RawResponse {
                status: 200,
                body: br#"{
                    "choices":[{"message":{"content":"hi"}}],
                    "usage":{"prompt_tokens":42,"completion_tokens":7,"cost":0.0013,
                             "prompt_tokens_details":{"cached_tokens":30}},
                    "provider":"Sail Research"
                }"#
                .to_vec(),
                retry_after_secs: None,
            })
        });
        let provider = CompletionsProvider::new(client, &ProviderConfig::default());
        let outcome = provider.chat("s", &[], &[]).await.unwrap();
        assert_eq!(outcome.usage.prompt_tokens, Some(42));
        assert_eq!(outcome.usage.cached_tokens, Some(30));
        assert_eq!(outcome.usage.completion_tokens, 7);
        assert_eq!(outcome.usage.cost, Some(0.0013));
        assert_eq!(outcome.usage.provider.as_deref(), Some("Sail Research"));
    }

    /// The endpoint name rides the response root; it must survive
    /// even when the response carries no usage block at all.
    #[tokio::test]
    async fn chat_captures_provider_without_usage() {
        let client = CompletionsClient::from_fn(|_body| async {
            Ok(crate::clients::RawResponse {
                status: 200,
                body: br#"{
                    "choices":[{"message":{"content":"hi"}}],
                    "provider":"Ambient"
                }"#
                .to_vec(),
                retry_after_secs: None,
            })
        });
        let provider = CompletionsProvider::new(client, &ProviderConfig::default());
        let outcome = provider.chat("s", &[], &[]).await.unwrap();
        assert_eq!(outcome.usage.prompt_tokens, None);
        assert_eq!(outcome.usage.provider.as_deref(), Some("Ambient"));
    }

    /// Usage without `prompt_tokens_details` must surface cache hits
    /// as absent, not zero — "can't know" and "cold prompt" differ.
    #[tokio::test]
    async fn chat_without_cache_details_leaves_cached_none() {
        let client = CompletionsClient::from_fn(|_body| async {
            Ok(crate::clients::RawResponse {
                status: 200,
                body: br#"{
                    "choices":[{"message":{"content":"hi"}}],
                    "usage":{"prompt_tokens":42,"completion_tokens":7}
                }"#
                .to_vec(),
                retry_after_secs: None,
            })
        });
        let provider = CompletionsProvider::new(client, &ProviderConfig::default());
        let outcome = provider.chat("s", &[], &[]).await.unwrap();
        assert_eq!(outcome.usage.cached_tokens, None);
    }

    #[tokio::test]
    async fn chat_without_usage_is_empty() {
        let client = CompletionsClient::from_fn(|_body| async {
            Ok(crate::clients::RawResponse {
                status: 200,
                body: br#"{"choices":[{"message":{"content":"hi"}}]}"#.to_vec(),
                retry_after_secs: None,
            })
        });
        let provider = CompletionsProvider::new(client, &ProviderConfig::default());
        let outcome = provider.chat("s", &[], &[]).await.unwrap();
        assert_eq!(outcome.usage.prompt_tokens, None);
        assert_eq!(outcome.usage.cost, None);
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
                        retry_after_secs: None,
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
        let outcome = provider.chat("s", &[], &[]).await.unwrap();
        assert!(matches!(outcome.response, Response::Text(t) if t == "hi"));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn chat_does_not_retry_fatal_errors() {
        use std::sync::atomic::Ordering;
        let (provider, calls) = flaky_provider(usize::MAX, ProviderError::BadRequest("400".into()));
        let err = provider.chat("s", &[], &[]).await.unwrap_err();
        assert!(matches!(err, ProviderError::BadRequest(_)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn chat_gives_up_after_max_retries() {
        use std::sync::atomic::Ordering;
        let (provider, calls) = flaky_provider(usize::MAX, ProviderError::Network("reset".into()));
        let err = provider.chat("s", &[], &[]).await.unwrap_err();
        assert!(matches!(err, ProviderError::Network(_)));
        assert_eq!(calls.load(Ordering::SeqCst), 1 + MAX_RETRIES as usize);
    }

    /// A 200 whose tool name cannot be transmitted back.
    const MALFORMED_NAME_BODY: &str = r#"{"choices":[{"message":{"content":"","tool_calls":[{"id":"c1","function":{"name":"review_disposition</arg_key>","arguments":"{}"}}]}}]}"#;

    /// A 200 with nothing in it.
    const EMPTY_CONTENT_BODY: &str = r#"{"choices":[{"message":{"content":""}}]}"#;

    /// Provider whose client answers 200 with `faulty` for the first
    /// `failures` calls and a usable text response after, counting
    /// every request. Unlike `flaky_provider` the failures are valid
    /// HTTP, so they surface from the parse rather than the transport.
    fn faulty_body_provider(
        failures: usize,
        faulty: &'static str,
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
            async move {
                Ok(crate::clients::RawResponse {
                    status: 200,
                    body: if n < failures {
                        faulty.as_bytes().to_vec()
                    } else {
                        br#"{"choices":[{"message":{"content":"hi"}}]}"#.to_vec()
                    },
                    retry_after_secs: None,
                })
            }
        });
        (
            CompletionsProvider::new(client, &ProviderConfig::default()),
            calls,
        )
    }

    #[test]
    fn policy_retries_a_malformed_tool_name() {
        let e = ProviderError::MalformedToolCall {
            name: "exec ".into(),
        };
        assert_eq!(retry_policy(&e, 0), Some(Duration::from_secs(1)));
    }

    #[tokio::test(start_paused = true)]
    async fn chat_redraws_after_a_malformed_tool_name() {
        use std::sync::atomic::Ordering;
        let (provider, calls) = faulty_body_provider(1, MALFORMED_NAME_BODY);
        let outcome = provider.chat("s", &[], &[]).await.unwrap();
        assert!(matches!(outcome.response, Response::Text(t) if t == "hi"));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn chat_gives_up_on_a_persistent_malformed_tool_name() {
        use std::sync::atomic::Ordering;
        let (provider, calls) = faulty_body_provider(usize::MAX, MALFORMED_NAME_BODY);
        let err = provider.chat("s", &[], &[]).await.unwrap_err();
        assert!(matches!(err, ProviderError::MalformedToolCall { .. }));
        assert_eq!(calls.load(Ordering::SeqCst), 1 + MAX_RETRIES as usize);
    }

    /// Parsing moved inside the retry unit; that must not make every
    /// parse failure retryable. Only the policy decides.
    #[tokio::test(start_paused = true)]
    async fn chat_does_not_redraw_an_empty_response() {
        use std::sync::atomic::Ordering;
        let (provider, calls) = faulty_body_provider(usize::MAX, EMPTY_CONTENT_BODY);
        let err = provider.chat("s", &[], &[]).await.unwrap_err();
        assert!(matches!(err, ProviderError::EmptyResponse));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn chat_rate_limit_waits_longer() {
        let start = tokio::time::Instant::now();
        let (provider, _) = flaky_provider(usize::MAX, ProviderError::RateLimited);
        provider.chat("s", &[], &[]).await.unwrap_err();
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

    /// The exact name Azure emitted for `openai/gpt-5.6-luna`: argument
    /// markup spliced into the function name.
    #[test]
    fn tool_name_with_leaked_argument_markup_is_refused() {
        let bad = "review_disposition</arg_key><arg_value>fixed</arg_value>";
        let result = CompletionsProvider::parse_response(response(AssistantMessage {
            content: Some("Now record the review dispositions:".to_string()),
            tool_calls: Some(vec![ApiToolCall {
                id: "call-1".to_string(),
                function: ApiFunction {
                    name: bad.to_string(),
                    arguments: r#"{"finding_id": 11}"#.to_string(),
                },
            }]),
            reasoning: None,
        }));
        assert!(matches!(
            result,
            Err(ProviderError::MalformedToolCall { name }) if name == bad
        ));
    }

    /// One bad name condemns the batch: a partial push would leave the
    /// good calls without results.
    #[test]
    fn one_malformed_name_refuses_the_whole_batch() {
        let mut bad = tool_call();
        bad.function.name = "exec ".to_string();
        let result = CompletionsProvider::parse_response(response(AssistantMessage {
            content: None,
            tool_calls: Some(vec![tool_call(), bad]),
            reasoning: None,
        }));
        assert!(matches!(
            result,
            Err(ProviderError::MalformedToolCall { .. })
        ));
    }

    #[test]
    fn empty_tool_name_is_refused() {
        let mut bad = tool_call();
        bad.function.name = String::new();
        let result = CompletionsProvider::parse_response(response(AssistantMessage {
            content: None,
            tool_calls: Some(vec![bad]),
            reasoning: None,
        }));
        assert!(matches!(
            result,
            Err(ProviderError::MalformedToolCall { .. })
        ));
    }

    /// Namespaced MCP tools carry `_` and `-`; both are legal.
    #[test]
    fn namespaced_mcp_tool_names_are_accepted() {
        let mut ok = tool_call();
        ok.function.name = "bkb_lookup-bip".to_string();
        let result = CompletionsProvider::parse_response(response(AssistantMessage {
            content: None,
            tool_calls: Some(vec![ok]),
            reasoning: None,
        }));
        assert!(matches!(result, Ok(Response::ToolCalls { .. })));
    }
}
