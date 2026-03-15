//! Tool execution system.
//!
//! Tools are functions the agent can call (exec, `read_file`, `web_search`, etc.).

pub(crate) mod cli_runner;
mod exec;
mod file_edit;
mod file_read;
mod file_write;
pub(crate) mod git;
pub(crate) mod github;
mod glob_search;
mod grep;
#[cfg(test)]
mod mock;
#[cfg(not(feature = "mock-network"))]
pub(crate) mod network;

pub mod path;

use exec::Exec;
use file_edit::FileEdit;
use file_read::FileRead;
use file_write::FileWrite;
use glob_search::GlobSearch;
use grep::Grep;

#[cfg(test)]
pub use mock::MockTool;

use std::borrow::Cow;
use std::ffi::OsString;
use std::future::Future;
use std::pin::Pin;

use crate::config::Config;
use crate::error::{ConfigError, ToolError};
use crate::types::{ToolCall, ToolDefinition};
use crate::workspace::Workspace;

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
    // Shell init (direnv hook via BASH_ENV)
    "BASH_ENV",
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
    // GPG
    "GNUPGHOME",
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
#[derive(Default)]
pub struct Tools(Vec<Box<dyn Tool>>);

impl Tools {
    /// Create a tool collection, filtering out any tools whose name
    /// appears in `disabled`.
    ///
    /// Returns an error if `disabled` contains a name that doesn't
    /// match any tool — this catches typos in the config.
    pub fn new(tools: Vec<Box<dyn Tool>>, disabled: &[String]) -> Result<Self, ConfigError> {
        if disabled.is_empty() {
            return Ok(Self(tools));
        }
        for name in disabled {
            if !tools.iter().any(|t| t.name() == name.as_str()) {
                return Err(ConfigError::Invalid(format!(
                    "tools.disabled: unknown tool \"{name}\""
                )));
            }
        }
        Ok(Self(
            tools
                .into_iter()
                .filter(|t| !disabled.iter().any(|d| d == t.name()))
                .collect(),
        ))
    }

    /// Build the set of local (non-network) tools.
    pub fn local(workspace: &Workspace, config: &Config) -> Vec<Box<dyn Tool>> {
        let guard = path::PathGuard::new(workspace.path());

        vec![
            Box::new(Exec::new(workspace.path(), &config.tools.exec)),
            Box::new(FileRead::new(guard.clone())),
            Box::new(FileWrite::new(guard.clone())),
            Box::new(FileEdit::new(guard.clone())),
            Box::new(GlobSearch::new(workspace.path())),
            Box::new(Grep::new(guard)),
        ]
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
        let tools = Tools::new(vec![Box::new(MockTool::new("ok"))], &[]).unwrap();
        let defs = tools.definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].function.name, "mock");
    }

    #[tokio::test]
    async fn test_execute() {
        let tools = Tools::new(vec![Box::new(MockTool::new("executed"))], &[]).unwrap();
        let result = tools.execute(&mock_call("test-123")).await.unwrap();
        assert_eq!(result, "executed");
    }

    #[tokio::test]
    async fn test_not_found() {
        let tools = Tools::default();
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
        let tools = Tools::new(vec![Box::new(MockTool::new("ok"))], &[]).unwrap();
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

    #[test]
    fn disabled_tools_filtered() {
        let tools = Tools::new(vec![Box::new(MockTool::new("ok"))], &["mock".to_string()]).unwrap();
        assert!(tools.definitions().is_empty());
    }

    #[test]
    fn disabled_unknown_name_rejected() {
        let result = Tools::new(
            vec![Box::new(MockTool::new("ok"))],
            &["nonexistent".to_string()],
        );
        match result {
            Err(ConfigError::Invalid(msg)) => assert!(msg.contains("nonexistent")),
            _ => panic!("expected ConfigError::Invalid"),
        }
    }
}
