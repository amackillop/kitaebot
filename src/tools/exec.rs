//! Shell command execution tool.
//!
//! Executes commands via `sh -c` within the workspace directory. This is the
//! primary mechanism for the agent to interact with the system.
//!
//! # Safety
//!
//! Commands are checked against a static deny list before execution:
//! - Recursive deletion (`rm -r`, `rm -rf`)
//! - Filesystem creation (`mkfs`)
//! - Raw disk writes (`dd if=`)
//! - Device writes (`> /dev/`)
//! - System power (`shutdown`, `reboot`)
//! - Fork bombs
//!
//! Path traversal (`../`) is also blocked to confine execution to the workspace.
//!
//! These are heuristics, not a sandbox. A determined attacker can bypass them.
//! Real isolation requires OS-level sandboxing (namespaces, seccomp, landlock).

use std::borrow::Cow;
use std::ffi::OsString;
use std::fmt::Write;
use std::path::PathBuf;
use std::sync::LazyLock;

use regex::RegexSet;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::process::Command;
use tokio::time::{Duration, timeout};
use tracing::{debug, warn};

use std::future::Future;
use std::pin::Pin;

use super::Tool;
use crate::config::ExecConfig;
use crate::error::ToolError;

/// Patterns that indicate dangerous commands.
static DENY_PATTERNS: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSet::new([
        r"rm\s+-[rf]",          // rm -r, rm -rf
        r"mkfs",                // filesystem creation
        r"dd\s+if=",            // disk operations
        r"(^|[^0-9])>\s*/dev/", // write to devices (not fd redirects like 2>/dev/null)
        r"shutdown|reboot",     // system power
        r":\(\)\s*\{.*\};\s*:", // fork bomb
    ])
    .expect("invalid deny patterns")
});

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
fn safe_env() -> impl Iterator<Item = (OsString, OsString)> {
    std::env::vars_os().filter(|(key, _)| key.to_str().is_some_and(|k| SAFE_ENV_VARS.contains(&k)))
}

/// Arguments for the exec tool.
#[derive(Deserialize, JsonSchema)]
struct Args {
    /// The shell command to execute.
    command: String,
}

/// Tool that executes shell commands in the workspace.
pub struct Exec {
    working_dir: PathBuf,
    timeout: Duration,
    max_output_bytes: usize,
}

impl Exec {
    pub fn new(working_dir: impl Into<PathBuf>, config: &ExecConfig) -> Self {
        Self {
            working_dir: working_dir.into(),
            timeout: Duration::from_secs(config.timeout_secs),
            max_output_bytes: config.max_output_bytes,
        }
    }
}

impl Tool for Exec {
    fn name(&self) -> &'static str {
        "exec"
    }

    fn description(&self) -> &'static str {
        "Execute a shell command in the workspace"
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

            if has_path_traversal(&args.command) {
                warn!(command = %args.command, "Path traversal detected");
                return Err(ToolError::Blocked("path traversal detected".into()));
            }

            if is_blocked(&args.command) {
                warn!(command = %args.command, "Command matches deny pattern");
                return Err(ToolError::Blocked("command matches deny pattern".into()));
            }

            debug!(command = %args.command, "Executing command");

            let output = timeout(
                self.timeout,
                Command::new("/bin/sh")
                    .arg("-c")
                    .arg(&args.command)
                    .current_dir(&self.working_dir)
                    .env_clear()
                    .envs(safe_env())
                    .output(),
            )
            .await
            .map_err(|_| ToolError::Timeout)?
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            let mut result = format!("$ {}\n", args.command);

            if !stdout.is_empty() {
                result.push_str(&truncate_output(&stdout, self.max_output_bytes));
            }

            if !stderr.is_empty() {
                if !stdout.is_empty() {
                    result.push('\n');
                }
                result.push_str("STDERR:\n");
                result.push_str(&truncate_output(&stderr, self.max_output_bytes));
            }

            let _ = write!(
                result,
                "\nExit code: {}",
                output.status.code().unwrap_or(-1)
            );

            Ok(result)
        })
    }
}

/// Truncate string at byte boundary without splitting UTF-8.
fn truncate_output(s: &str, max_bytes: usize) -> Cow<'_, str> {
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

/// Check if command contains path traversal.
fn has_path_traversal(cmd: &str) -> bool {
    cmd.contains("../")
}

/// Check if command matches any deny pattern.
fn is_blocked(cmd: &str) -> bool {
    DENY_PATTERNS.is_match(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parameters_schema() {
        let tool = Exec::new(".", &ExecConfig::default());
        let schema = tool.parameters();

        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["command"]["type"], "string");
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("command"))
        );
    }

    #[test]
    fn test_deny_patterns() {
        assert!(is_blocked("rm -rf /"));
        assert!(is_blocked("rm -r foo"));
        assert!(is_blocked("mkfs.ext4 /dev/sda"));
        assert!(is_blocked("dd if=/dev/zero of=/dev/sda"));
        assert!(is_blocked("echo foo > /dev/sda"));
        assert!(is_blocked("shutdown now"));
        assert!(is_blocked("reboot"));
        assert!(is_blocked(":() { :|:& }; :"));

        assert!(!is_blocked("ls -la"));
        assert!(!is_blocked("cat foo.txt"));
        assert!(!is_blocked("echo hello"));
        assert!(!is_blocked("find / -name justfile 2>/dev/null"));
    }

    #[test]
    fn test_path_traversal() {
        assert!(has_path_traversal("cat ../secret"));
        assert!(has_path_traversal("ls ../../"));

        assert!(!has_path_traversal("ls ./foo"));
        assert!(!has_path_traversal("cat /etc/passwd"));
    }

    #[test]
    fn test_truncate_output() {
        let short = "hello";
        assert_eq!(truncate_output(short, 100), Cow::Borrowed("hello"));

        let long = "a".repeat(100);
        let truncated = truncate_output(&long, 10);
        assert!(truncated.ends_with("[truncated 90 bytes]"));
        assert!(truncated.starts_with("aaaaaaaaaa"));
    }

    #[tokio::test]
    async fn test_exec_simple_command() {
        let tool = Exec::new(".", &ExecConfig::default());
        let args = serde_json::json!({"command": "echo hello"});
        let result = tool.execute(args).await.unwrap();
        assert!(result.contains("hello"));
        assert!(result.contains("Exit code: 0"));
    }

    #[tokio::test]
    async fn test_exec_missing_command() {
        let tool = Exec::new(".", &ExecConfig::default());
        let args = serde_json::json!({});
        let result = tool.execute(args).await;
        assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
    }

    #[tokio::test]
    async fn test_exec_blocked_command() {
        let tool = Exec::new(".", &ExecConfig::default());
        let args = serde_json::json!({"command": "rm -rf /"});
        let result = tool.execute(args).await;
        assert!(matches!(result, Err(ToolError::Blocked(_))));
    }

    #[tokio::test]
    async fn test_exec_path_traversal_blocked() {
        let tool = Exec::new(".", &ExecConfig::default());
        let args = serde_json::json!({"command": "cat ../secret"});
        let result = tool.execute(args).await;
        assert!(matches!(result, Err(ToolError::Blocked(_))));
    }

    #[tokio::test]
    async fn test_exec_env_scrubbed() {
        // Set a variable that is NOT on the allowlist
        // SAFETY: test-only, no concurrent threads depend on this var.
        unsafe { std::env::set_var("KITAEBOT_TEST_SECRET", "leaked") };
        let tool = Exec::new(".", &ExecConfig::default());
        let args = serde_json::json!({"command": "echo $KITAEBOT_TEST_SECRET"});
        let result = tool.execute(args).await.unwrap();
        // Shell expands unset vars to empty string, so output should just be a blank line
        assert!(
            !result.contains("leaked"),
            "secret leaked through env: {result}"
        );
        unsafe { std::env::remove_var("KITAEBOT_TEST_SECRET") };
    }

    #[tokio::test]
    async fn test_exec_path_available() {
        let tool = Exec::new(".", &ExecConfig::default());
        let args = serde_json::json!({"command": "echo $PATH"});
        let result = tool.execute(args).await.unwrap();
        // PATH should be forwarded — output should contain something (not just "$ echo $PATH\n\n")
        let lines: Vec<&str> = result.lines().collect();
        // Line 0 is "$ echo $PATH", line 1 is the actual PATH value
        assert!(lines.len() >= 2, "expected PATH output: {result}");
        assert!(!lines[1].is_empty(), "PATH was empty: {result}");
    }
}
