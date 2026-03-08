//! Tool execution system.
//!
//! Tools are functions the agent can call (exec, `read_file`, `web_search`, etc.).

mod exec;
mod file_edit;
mod file_read;
mod file_write;
#[cfg(not(feature = "mock-network"))]
mod github;
mod glob_search;
mod grep;
#[cfg(test)]
mod mock;
pub mod path;
#[cfg(not(feature = "mock-network"))]
mod web_fetch;
#[cfg(not(feature = "mock-network"))]
mod web_search;

pub use exec::Exec;
pub use file_edit::FileEdit;
pub use file_read::FileRead;
pub use file_write::FileWrite;
#[cfg(not(feature = "mock-network"))]
pub use github::GitHub;
pub use glob_search::GlobSearch;
pub use grep::Grep;
#[cfg(test)]
pub use mock::MockTool;
#[cfg(not(feature = "mock-network"))]
pub use web_fetch::WebFetch;
#[cfg(not(feature = "mock-network"))]
pub use web_search::WebSearch;

use std::borrow::Cow;
use std::ffi::OsString;
use std::future::Future;
use std::pin::Pin;

use crate::error::ToolError;
use crate::types::{ToolCall, ToolDefinition};

/// Environment variables forwarded to child processes.
///
/// Everything else is scrubbed. Notably absent: `CREDENTIALS_DIRECTORY`.
const SAFE_ENV_VARS: &[&str] = &[
    // Execution
    "PATH",
    "HOME",
    "USER",
    "SHELL",
    // Locale
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    // Terminal
    "TERM",
    "COLORTERM",
    // Temp
    "TMPDIR",
    "TMP",
    "TEMP",
    // Nix
    "NIX_PATH",
    "NIX_PROFILES",
    "NIX_SSL_CERT_FILE",
    // TLS
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "CURL_CA_BUNDLE",
    // Workspace
    "KITAEBOT_WORKSPACE",
    // Misc
    "TZ",
    "EDITOR",
    "VISUAL",
    // XDG
    "XDG_DATA_HOME",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "XDG_RUNTIME_DIR",
];

/// Build a filtered environment from the current process, keeping only known-safe variables.
pub(crate) fn safe_env() -> impl Iterator<Item = (OsString, OsString)> {
    std::env::vars_os().filter(|(key, _)| key.to_str().is_some_and(|k| SAFE_ENV_VARS.contains(&k)))
}

/// A tool the agent can invoke.
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn parameters(&self) -> serde_json::Value;
    fn execute(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>>;
}

/// Collection of available tools.
///
/// Uses `Vec` with linear scan for lookup. For small tool counts (<50),
/// this outperforms `HashMap` due to cache locality and no hashing overhead.
/// Tool execution involves HTTP calls to an LLM (100ms+), so lookup time is noise.
pub struct Tools(Vec<Box<dyn Tool>>);

impl Tools {
    pub fn new(tools: Vec<Box<dyn Tool>>) -> Self {
        Self(tools)
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.0
            .iter()
            .map(|t| {
                ToolDefinition::new(
                    t.name().to_string(),
                    t.description().to_string(),
                    t.parameters(),
                )
            })
            .collect()
    }

    pub async fn execute(&self, call: &ToolCall) -> Result<String, ToolError> {
        let tool = self
            .0
            .iter()
            .find(|t| t.name() == call.function.name)
            .ok_or_else(|| ToolError::NotFound(call.function.name.clone()))?;

        let args: serde_json::Value = serde_json::from_str(&call.function.arguments)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        tool.execute(args).await
    }
}

impl Default for Tools {
    fn default() -> Self {
        Self::new(vec![])
    }
}

/// Truncate string at byte boundary without splitting UTF-8.
///
/// If `s` exceeds `max_bytes`, it is cut at the nearest character boundary
/// and a summary of dropped bytes is appended.
pub(crate) fn truncate_output(s: &str, max_bytes: usize) -> Cow<'_, str> {
    if s.len() <= max_bytes {
        Cow::Borrowed(s)
    } else {
        let end = s.floor_char_boundary(max_bytes);
        Cow::Owned(format!(
            "{}...\n[truncated {} bytes]",
            &s[..end],
            s.len() - end
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolFunction;

    fn mock_call(id: &str) -> ToolCall {
        ToolCall::new(
            id.to_string(),
            ToolFunction {
                name: "mock".to_string(),
                arguments: "{}".to_string(),
            },
        )
    }

    #[test]
    fn test_definitions() {
        let tools = Tools::new(vec![Box::new(MockTool::new("ok"))]);
        let defs = tools.definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].function.name, "mock");
    }

    #[tokio::test]
    async fn test_execute() {
        let tools = Tools::new(vec![Box::new(MockTool::new("executed"))]);
        let result = tools.execute(&mock_call("test-123")).await.unwrap();
        assert_eq!(result, "executed");
    }

    #[tokio::test]
    async fn test_not_found() {
        let tools = Tools::new(vec![]);
        let call = ToolCall::new(
            "test-123".to_string(),
            ToolFunction {
                name: "nonexistent".to_string(),
                arguments: "{}".to_string(),
            },
        );
        let result = tools.execute(&call).await;
        assert!(matches!(result.unwrap_err(), ToolError::NotFound(_)));
    }

    #[test]
    fn truncate_short_string_borrowed() {
        assert!(matches!(
            truncate_output("hello", 100),
            Cow::Borrowed("hello")
        ));
    }

    #[test]
    fn truncate_exact_length_borrowed() {
        assert!(matches!(
            truncate_output("hello", 5),
            Cow::Borrowed("hello")
        ));
    }

    #[test]
    fn truncate_long_string() {
        let long = "a".repeat(100);
        let result = truncate_output(&long, 10);
        assert!(result.starts_with("aaaaaaaaaa"));
        assert!(result.ends_with("[truncated 90 bytes]"));
    }

    #[test]
    fn truncate_utf8_boundary() {
        // '€' is 3 bytes. Truncating at byte 2 should cut back to 0.
        let result = truncate_output("€", 2);
        assert!(result.starts_with("...\n[truncated 3 bytes]"));
    }

    #[tokio::test]
    async fn test_invalid_arguments() {
        let tools = Tools::new(vec![Box::new(MockTool::new("ok"))]);
        let call = ToolCall::new(
            "test-123".to_string(),
            ToolFunction {
                name: "mock".to_string(),
                arguments: "invalid json".to_string(),
            },
        );
        let result = tools.execute(&call).await;
        assert!(matches!(
            result.unwrap_err(),
            ToolError::InvalidArguments(_)
        ));
    }
}
