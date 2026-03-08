//! Web fetch tool.
//!
//! Fetches a URL and returns the response body as text. HTML tags are stripped
//! and whitespace collapsed so the LLM gets clean prose, not markup.

use std::future::Future;
use std::pin::Pin;
use std::sync::LazyLock;

use regex::Regex;
use reqwest::Client;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::time::Duration;
use tracing::{debug, warn};

use super::Tool;
use crate::config::WebFetchConfig;
use crate::error::ToolError;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// URL to fetch. Must be http or https.
    url: String,
}

/// Tool that fetches content from a URL.
pub struct WebFetch {
    client: Client,
    timeout: Duration,
    max_response_bytes: usize,
}

impl WebFetch {
    pub fn new(config: &WebFetchConfig) -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: Client::builder().build()?,
            timeout: Duration::from_secs(config.timeout_secs),
            max_response_bytes: config.max_response_bytes,
        })
    }
}

impl Tool for WebFetch {
    fn name(&self) -> &'static str {
        "web_fetch"
    }

    fn description(&self) -> &'static str {
        "Fetch content from a URL and return it as text"
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

            validate_url(&args.url)?;
            debug!(url = %args.url, "Fetching URL");

            let response = tokio::time::timeout(self.timeout, self.client.get(&args.url).send())
                .await
                .map_err(|_| ToolError::Timeout)?
                .map_err(|e| ToolError::ExecutionFailed(format!("fetch failed: {e}")))?;

            let status = response.status();
            if !status.is_success() {
                warn!(url = %args.url, %status, "Fetch failed");
                return Err(ToolError::ExecutionFailed(format!("HTTP {status}")));
            }

            let body = response
                .text()
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("failed to read body: {e}")))?;

            let text = strip_html(&body);
            Ok(super::truncate_output(&text, self.max_response_bytes).into_owned())
        })
    }
}

/// Reject anything that isn't http or https.
fn validate_url(url: &str) -> Result<(), ToolError> {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        Ok(())
    } else {
        Err(ToolError::InvalidArguments(
            "URL must use http or https scheme".into(),
        ))
    }
}

static TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]*>").expect("static regex"));
static WS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").expect("static regex"));

/// Strip HTML tags and collapse whitespace into clean text.
fn strip_html(html: &str) -> String {
    let no_tags = TAG_RE.replace_all(html, " ");
    WS_RE.replace_all(&no_tags, " ").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_http() {
        assert!(validate_url("http://example.com").is_ok());
        assert!(validate_url("https://example.com").is_ok());
        assert!(validate_url("HTTP://EXAMPLE.COM").is_ok());
    }

    #[test]
    fn reject_non_http() {
        assert!(validate_url("ftp://example.com").is_err());
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("javascript:alert(1)").is_err());
        assert!(validate_url("not a url").is_err());
    }

    #[test]
    fn strip_html_tags() {
        let html = "<html><body><h1>Hello</h1><p>World</p></body></html>";
        assert_eq!(strip_html(html), "Hello World");
    }

    #[test]
    fn strip_html_preserves_text() {
        assert_eq!(strip_html("no tags here"), "no tags here");
    }

    #[test]
    fn strip_html_collapses_whitespace() {
        let html = "<p>hello</p>\n\n\n<p>world</p>";
        assert_eq!(strip_html(html), "hello world");
    }
}
