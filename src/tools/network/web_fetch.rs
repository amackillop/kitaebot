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

use super::{Tool, ToolCtx};
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
        crate::tools::schema_of::<Args>()
    }

    fn execute(
        &self,
        args: serde_json::Value,
        _ctx: ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let args: Args = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

            validate_url(&args.url)?;
            debug!(url = %args.url, "Fetching URL");

            let response = tokio::time::timeout(self.timeout, self.client.get(&args.url).send())
                .await
                .map_err(|_| ToolError::Timeout {
                    command: format!("fetch {}", args.url),
                    secs: self.timeout.as_secs(),
                    evidence: crate::error::TimeoutEvidence::default(),
                })?
                .map_err(|e| ToolError::Http {
                    url: args.url.clone(),
                    source: e,
                })?;

            let status = response.status();
            if !status.is_success() {
                warn!(url = %args.url, %status, "Fetch failed");
                if let Some(guidance) = github_guidance(&args.url, status.as_u16()) {
                    return Err(ToolError::Blocked {
                        operation: format!("fetch {} (HTTP {status})", args.url),
                        guidance,
                    });
                }
                return Err(ToolError::HttpStatus {
                    url: args.url,
                    status: status.as_u16(),
                });
            }

            let body = response.text().await.map_err(|e| ToolError::Http {
                url: args.url,
                source: e,
            })?;

            let text = strip_html(&body);
            Ok(super::truncate_output(&text, self.max_response_bytes).into_owned())
        })
    }
}

/// GitHub answers 404 (not 403) for private resources on
/// unauthenticated requests, so the status alone reads as "does not
/// exist" and invites URL-variant retries. Steer to the tools that
/// carry the token.
fn github_guidance(url: &str, status: u16) -> Option<String> {
    if !matches!(status, 403 | 404) {
        return None;
    }
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    let github = host == "github.com"
        || host.ends_with(".github.com")
        || host.ends_with(".githubusercontent.com");
    github.then(|| {
        "web_fetch is unauthenticated, so private GitHub resources \
         return 404. Use the github_* tools (github_api, \
         github_ci_status, github_pr_*) for workspace repos, or \
         file_read on the local checkout."
            .to_string()
    })
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

static TAG_RE: LazyLock<Regex> = LazyLock::new(|| crate::text::static_regex(r"<[^>]*>"));
static WS_RE: LazyLock<Regex> = LazyLock::new(|| crate::text::static_regex(r"\s+"));

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
    fn github_hosts_get_guidance_on_missing() {
        for url in [
            "https://github.com/o/r/pull/912",
            "https://api.github.com/repos/o/r",
            "https://gist.github.com/o/abc",
            "https://raw.githubusercontent.com/o/r/main/x.rs",
        ] {
            assert!(github_guidance(url, 404).is_some(), "{url} should steer");
            assert!(github_guidance(url, 403).is_some(), "{url} should steer");
        }
    }

    #[test]
    fn non_auth_statuses_pass_through() {
        for status in [400, 429, 500, 503] {
            assert!(github_guidance("https://github.com/o/r", status).is_none());
        }
    }

    #[test]
    fn non_github_hosts_pass_through() {
        for url in [
            "https://example.com/github.com/o/r",
            "https://evilgithub.com/o/r",
            "https://github.com.evil.example/o/r",
            "not a url",
        ] {
            assert!(
                github_guidance(url, 404).is_none(),
                "{url} should not steer"
            );
        }
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
