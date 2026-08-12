//! Tool execution system.
//!
//! Tools are functions the agent can call (exec, `read_file`, `web_search`, etc.).

mod bwrap;
pub(crate) mod cli_runner;
pub(crate) mod direnv;
mod exec;
mod file_edit;
mod file_read;
mod file_write;
pub(crate) mod git;
pub(crate) mod github;
mod glob_search;
mod grep;
pub(crate) mod linear;
pub(crate) mod mcp;
#[cfg(test)]
mod mock;
#[cfg(not(feature = "mock-network"))]
pub(crate) mod network;
pub(crate) mod warm;

pub mod path;

use exec::Exec;
use file_edit::FileEdit;
use file_read::FileRead;
use file_write::FileWrite;
use glob_search::GlobSearch;
use grep::Grep;

#[cfg(test)]
pub use mock::{MockBlockedTool, MockTool};

use std::borrow::Cow;
use std::ffi::OsString;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::activity::Activity;
use crate::config::Config;
use crate::error::{ConfigError, ToolError};
use crate::types::{ToolCall, ToolDefinition};
use crate::workspace::Workspace;

pub use direnv::DirenvCache;
pub use warm::Warmer;

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
    // Egress proxy (spec 18) — children must inherit the proxy or the
    // firewall drops their traffic. Both cases: tooling disagrees.
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    // Workspace
    "KITAEBOT_WORKSPACE",
    // Build
    "CARGO_TARGET_DIR",
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

/// Per-turn context threaded into every tool execution.
///
/// Owned and cheap to clone (both fields are Arc-backed); the agent
/// loop clones it once per tool call, and the clone moves into the
/// tool's boxed future. Most tools ignore it — the `task` tool uses
/// both fields to forward child activity and propagate cancellation.
///
/// Primary cancellation remains drop-based (the loop races `join_all`
/// against the token); the token here is for tools that can react more
/// gracefully than being dropped.
#[derive(Clone)]
pub struct ToolCtx {
    /// Activity event sink; `None` when no observer is attached.
    pub activity: Option<mpsc::Sender<Activity>>,
    /// Cancelled when the client disconnects mid-turn.
    pub cancel: CancellationToken,
}

impl Default for ToolCtx {
    /// A context with no observer and a token that never fires.
    fn default() -> Self {
        Self {
            activity: None,
            cancel: CancellationToken::new(),
        }
    }
}

/// A tool the agent can invoke.
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn parameters(&self) -> serde_json::Value;
    fn execute(
        &self,
        args: serde_json::Value,
        ctx: ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>>;
}

/// Collection of available tools.
///
/// Uses `Vec` with linear scan for lookup. For small tool counts (<50),
/// this outperforms `HashMap` due to cache locality and no hashing overhead.
/// Tool execution involves HTTP calls to an LLM (100ms+), so lookup time is noise.
///
/// Tools are held via `Arc` so a single instance can appear in
/// multiple sets (root agent, sub-agent allowlists).
#[derive(Default, Clone)]
pub struct Tools(Vec<Arc<dyn Tool>>);

impl Tools {
    /// Create a tool collection, filtering out any tools whose name
    /// appears in `disabled`.
    ///
    /// Returns an error if `disabled` contains a name that doesn't
    /// match any tool — this catches typos in the config.
    pub fn new(tools: Vec<Arc<dyn Tool>>, disabled: &[String]) -> Result<Self, ConfigError> {
        // Registered names ship in tools[] on every request; one the
        // API's grammar rejects would 400 every call the daemon makes.
        // Built-ins are literals and MCP names are checked at their own
        // registration, so a failure here is a programming error caught
        // at startup.
        for tool in &tools {
            if !crate::types::is_valid_tool_name(tool.name()) {
                return Err(ConfigError::Invalid(format!(
                    "tool name {:?} must match ^[a-zA-Z0-9_-]+$",
                    tool.name()
                )));
            }
        }
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

    /// Append additional tools, applying the same `disabled` filter
    /// used at construction. Engine-contributed tools are merged in
    /// after `Tools::new` because the engine is built later in
    /// startup than the static tool registry.
    pub fn extend_with(&mut self, more: Vec<Arc<dyn Tool>>, disabled: &[String]) {
        for tool in more {
            // Engine tools are our own literals; a bad one is a typo
            // no test suite should let boot.
            assert!(
                crate::types::is_valid_tool_name(tool.name()),
                "tool name {:?} must match ^[a-zA-Z0-9_-]+$",
                tool.name()
            );
            if !disabled.iter().any(|d| d == tool.name()) {
                self.0.push(tool);
            }
        }
    }

    /// Whether a tool with this name is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.0.iter().any(|t| t.name() == name)
    }

    /// Project this collection onto an allowlist of tool names.
    ///
    /// Names with no matching tool are skipped: an allowlisted tool
    /// may be absent because the operator disabled it via
    /// `tools.disabled` or because it was compiled out (mock-network).
    /// Typos in the hardcoded allowlists are caught by tests, not at
    /// runtime.
    pub fn filtered(&self, allow: &[&str]) -> Self {
        Self(
            allow
                .iter()
                .filter_map(|name| self.0.iter().find(|t| t.name() == *name).cloned())
                .collect(),
        )
    }

    /// Build the set of local (non-network) tools.
    pub fn local(
        workspace: &Workspace,
        config: &Config,
        direnv: DirenvCache,
    ) -> Vec<Arc<dyn Tool>> {
        let guard = path::PathGuard::new(workspace.path());

        vec![
            Arc::new(Exec::new(
                workspace.path(),
                &config.tools.exec,
                direnv,
                config.git.trusted_repos(),
            )),
            Arc::new(FileRead::new(guard.clone())),
            Arc::new(FileWrite::new(guard.clone())),
            Arc::new(FileEdit::new(guard.clone())),
            Arc::new(GlobSearch::new(workspace.path())),
            Arc::new(Grep::new(guard)),
        ]
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.0
            .iter()
            .map(|t| {
                ToolDefinition::new(
                    t.name()
                        .parse()
                        .expect("registration checks the tool-name grammar"),
                    t.description().to_string(),
                    t.parameters(),
                )
            })
            .collect()
    }

    /// Dispatch a tool call, logging start/end with duration. Results
    /// are recorded by the agent loop only after all parallel calls
    /// finish, so these events are the accurate per-call timeline.
    pub async fn execute(&self, call: &ToolCall, ctx: ToolCtx) -> Result<String, ToolError> {
        tracing::debug!(tool = %call.function.name, call_id = %call.id, "Tool started");
        let started = std::time::Instant::now();
        let result = self.dispatch(call, ctx).await;
        let elapsed = started.elapsed();
        match &result {
            Ok(output) => tracing::debug!(
                tool = %call.function.name,
                call_id = %call.id,
                ?elapsed,
                bytes = output.len(),
                "Tool finished"
            ),
            Err(e) => tracing::debug!(
                tool = %call.function.name,
                call_id = %call.id,
                ?elapsed,
                error = %e,
                "Tool failed"
            ),
        }
        result
    }

    async fn dispatch(&self, call: &ToolCall, ctx: ToolCtx) -> Result<String, ToolError> {
        let tool = self
            .0
            .iter()
            .find(|t| t.name() == call.function.name.as_str())
            .ok_or_else(|| ToolError::NotFound(call.function.name.to_string()))?;

        let args: serde_json::Value = serde_json::from_str(&call.function.arguments)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        tool.execute(args, ctx).await
    }
}

/// Memory-protection ceiling for tool output, in bytes.
///
/// Not configurable: it protects the daemon from a runaway command
/// filling RAM, not the context window. Context-size policy lives in
/// the engines (`context.tool_output_tokens`), which see the output
/// long before it approaches this ceiling.
pub(crate) const TOOL_OUTPUT_CEILING_BYTES: usize = 5 * 1024 * 1024;

/// Deserialize `Option<T>` tolerating a JSON string where a
/// value/null is expected (LLM double-encoding).
pub(crate) fn string_or_value<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: serde::de::DeserializeOwned,
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => {
            let parsed: serde_json::Value =
                serde_json::from_str(&s).map_err(serde::de::Error::custom)?;
            match parsed {
                serde_json::Value::Null => Ok(None),
                other => serde_json::from_value(other)
                    .map(Some)
                    .map_err(serde::de::Error::custom),
            }
        }
        Some(other) => serde_json::from_value(other)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

/// Required-field counterpart of [`string_or_value`]. Only for
/// non-string targets: a string target would round-trip through the
/// JSON parser and reject unquoted content.
pub(crate) fn string_or_value_required<'de, T, D>(deserializer: D) -> Result<T, D::Error>
where
    T: serde::de::DeserializeOwned,
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::String(s) => {
            let parsed: serde_json::Value =
                serde_json::from_str(&s).map_err(serde::de::Error::custom)?;
            serde_json::from_value(parsed).map_err(serde::de::Error::custom)
        }
        other => serde_json::from_value(other).map_err(serde::de::Error::custom),
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
                name: "mock".parse().unwrap(),
                arguments: "{}".to_string(),
            },
        )
    }

    #[derive(serde::Deserialize)]
    struct RequiredInt {
        #[serde(deserialize_with = "string_or_value_required")]
        n: u64,
    }

    #[test]
    fn string_or_value_required_accepts_native_int() {
        let v: RequiredInt = serde_json::from_str(r#"{"n": 30}"#).unwrap();
        assert_eq!(v.n, 30);
    }

    #[test]
    fn string_or_value_required_accepts_string_encoded_int() {
        let v: RequiredInt = serde_json::from_str(r#"{"n": "30"}"#).unwrap();
        assert_eq!(v.n, 30);
    }

    #[test]
    fn string_or_value_required_rejects_garbage() {
        assert!(serde_json::from_str::<RequiredInt>(r#"{"n": "thirty"}"#).is_err());
        assert!(serde_json::from_str::<RequiredInt>(r#"{"n": null}"#).is_err());
    }

    #[test]
    fn test_definitions() {
        let tools = Tools::new(vec![Arc::new(MockTool::new("ok"))], &[]).unwrap();
        let defs = tools.definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].function.name, "mock");
    }

    #[tokio::test]
    async fn test_execute() {
        let tools = Tools::new(vec![Arc::new(MockTool::new("executed"))], &[]).unwrap();
        let result = tools
            .execute(&mock_call("test-123"), ToolCtx::default())
            .await
            .unwrap();
        assert_eq!(result, "executed");
    }

    #[tokio::test]
    async fn test_not_found() {
        let tools = Tools::default();
        let call = ToolCall::new(
            "test-123".to_string(),
            ToolFunction {
                name: "nonexistent".parse().unwrap(),
                arguments: "{}".to_string(),
            },
        );
        let result = tools.execute(&call, ToolCtx::default()).await;
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
        let tools = Tools::new(vec![Arc::new(MockTool::new("ok"))], &[]).unwrap();
        let call = ToolCall::new(
            "test-123".to_string(),
            ToolFunction {
                name: "mock".parse().unwrap(),
                arguments: "invalid json".to_string(),
            },
        );
        let result = tools.execute(&call, ToolCtx::default()).await;
        assert!(matches!(
            result.unwrap_err(),
            ToolError::InvalidArguments(_)
        ));
    }

    #[test]
    fn filtered_projects_allowlist_sharing_instances() {
        let mock: Arc<dyn Tool> = Arc::new(MockTool::new("ok"));
        let tools = Tools::new(vec![Arc::clone(&mock)], &[]).unwrap();
        let subset = tools.filtered(&["mock"]);
        assert_eq!(subset.definitions().len(), 1);
        assert!(Arc::ptr_eq(&mock, &subset.0[0]));
    }

    #[test]
    fn filtered_skips_missing_names() {
        let tools = Tools::new(vec![Arc::new(MockTool::new("ok"))], &[]).unwrap();
        let subset = tools.filtered(&["mock", "web_fetch"]);
        let defs = subset.definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].function.name, "mock");
    }

    #[test]
    fn filtered_empty_allowlist_is_empty() {
        let tools = Tools::new(vec![Arc::new(MockTool::new("ok"))], &[]).unwrap();
        assert!(tools.filtered(&[]).definitions().is_empty());
    }

    #[test]
    fn disabled_tools_filtered() {
        let tools = Tools::new(vec![Arc::new(MockTool::new("ok"))], &["mock".to_string()]).unwrap();
        assert!(tools.definitions().is_empty());
    }

    /// The registry is what feeds tools[]; a name the API's grammar
    /// rejects must fail at startup, not 400 every later request.
    #[test]
    fn invalid_tool_name_rejected_at_registration() {
        let result = Tools::new(vec![Arc::new(MockBlockedTool::named("bad name", "g"))], &[]);
        assert!(matches!(
            result,
            Err(crate::error::ConfigError::Invalid(msg)) if msg.contains("bad name")
        ));
    }

    #[test]
    fn disabled_unknown_name_rejected() {
        let result = Tools::new(
            vec![Arc::new(MockTool::new("ok"))],
            &["nonexistent".to_string()],
        );
        match result {
            Err(ConfigError::Invalid(msg)) => assert!(msg.contains("nonexistent")),
            _ => panic!("expected ConfigError::Invalid"),
        }
    }
}
