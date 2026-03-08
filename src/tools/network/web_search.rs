//! Web search tool.
//!
//! Searches the web via Perplexity (routed through `OpenRouter`) and returns
//! a synthesized answer.

use std::fmt::Write;
use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::time::Duration;
use tracing::{debug, warn};

use super::Tool;
use crate::chat_completion::{ChatCompletionsClient, CompletionsApi};
use crate::config::WebSearchConfig;
use crate::error::{ProviderError, ToolError};

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Search query.
    query: String,
}

/// Tool that searches the web via Perplexity on `OpenRouter`.
pub struct WebSearch {
    client: ChatCompletionsClient,
    model: String,
    max_tokens: u32,
    timeout: Duration,
}

impl WebSearch {
    pub fn new(client: ChatCompletionsClient, config: &WebSearchConfig) -> Self {
        Self {
            client,
            model: config.model.clone(),
            max_tokens: config.max_tokens,
            timeout: Duration::from_secs(config.timeout_secs),
        }
    }
}

impl Tool for WebSearch {
    fn name(&self) -> &'static str {
        "web_search"
    }

    fn description(&self) -> &'static str {
        "Search the web and return a synthesized answer"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(Args)).expect("schema serialization failed")
    }

    fn execute(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let args: Args = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

            debug!(query = %args.query, model = %self.model, "Searching web");

            let request = SearchRequest {
                model: &self.model,
                max_tokens: self.max_tokens,
                messages: &[RequestMessage {
                    role: "user",
                    content: &args.query,
                }],
            };

            let response =
                tokio::time::timeout(self.timeout, self.client.chat_completions(&request))
                    .await
                    .map_err(|_| ToolError::Timeout)?
                    .map_err(|e| match e {
                        ProviderError::Authentication => {
                            ToolError::ExecutionFailed("authentication failed".into())
                        }
                        ProviderError::RateLimited => {
                            ToolError::ExecutionFailed("rate limited".into())
                        }
                        ProviderError::InvalidResponse(msg) => {
                            ToolError::ExecutionFailed(format!("invalid response: {msg}"))
                        }
                        ProviderError::Network(msg) => {
                            warn!("Search API error: {msg}");
                            ToolError::ExecutionFailed(format!("search request failed: {msg}"))
                        }
                    })?;

            let mut answer = response
                .choices
                .into_iter()
                .next()
                .and_then(|c| c.message.content)
                .ok_or_else(|| {
                    ToolError::ExecutionFailed("no content in search response".into())
                })?;

            if !response.citations.is_empty() {
                answer.push_str("\n\nSources:\n");
                for (i, url) in response.citations.iter().enumerate() {
                    let _ = writeln!(answer, "[{}] {}", i + 1, url);
                }
            }

            Ok(answer)
        })
    }
}

// --- Wire format (request only — response types are in chat_completion.rs) ---

#[derive(Serialize)]
struct SearchRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: &'a [RequestMessage<'a>],
}

#[derive(Serialize)]
struct RequestMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_completion::ChatResponse;

    #[test]
    fn request_serialization() {
        let request = SearchRequest {
            model: "perplexity/sonar",
            max_tokens: 1024,
            messages: &[RequestMessage {
                role: "user",
                content: "what is rust?",
            }],
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["model"], "perplexity/sonar");
        assert_eq!(json["max_tokens"], 1024);
        assert_eq!(json["messages"][0]["role"], "user");
        assert_eq!(json["messages"][0]["content"], "what is rust?");
    }

    #[test]
    fn response_deserialization() {
        let json = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "Rust is a systems programming language."
                }
            }]
        });
        let response: ChatResponse = serde_json::from_value(json).unwrap();
        assert_eq!(
            response.choices[0].message.content.as_deref(),
            Some("Rust is a systems programming language.")
        );
        assert!(response.citations.is_empty());
    }

    #[test]
    fn response_with_citations() {
        let json = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "Rust is fast [1] and safe [2]."
                }
            }],
            "citations": [
                "https://www.rust-lang.org/",
                "https://doc.rust-lang.org/book/"
            ]
        });
        let response: ChatResponse = serde_json::from_value(json).unwrap();
        assert_eq!(response.citations.len(), 2);
        assert_eq!(response.citations[0], "https://www.rust-lang.org/");
        assert_eq!(response.citations[1], "https://doc.rust-lang.org/book/");
    }

    #[test]
    fn response_empty_choices() {
        let json = serde_json::json!({"choices": []});
        let response: ChatResponse = serde_json::from_value(json).unwrap();
        assert!(response.choices.is_empty());
    }

    #[test]
    fn response_null_content() {
        let json = serde_json::json!({
            "choices": [{"message": {"content": null}}]
        });
        let response: ChatResponse = serde_json::from_value(json).unwrap();
        assert!(response.choices[0].message.content.is_none());
    }
}
