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
use std::fmt::Write;
use std::path::PathBuf;
use std::sync::LazyLock;

use async_trait::async_trait;
use regex::RegexSet;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

use crate::error::ToolError;
use crate::tools::Tool;

const DEFAULT_TIMEOUT_SECS: u64 = 60;
const MAX_OUTPUT_BYTES: usize = 10 * 1024;

/// Patterns that indicate dangerous commands.
static DENY_PATTERNS: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSet::new([
        r"rm\s+-[rf]",          // rm -r, rm -rf
        r"mkfs",                // filesystem creation
        r"dd\s+if=",            // disk operations
        r">\s*/dev/",           // write to devices
        r"shutdown|reboot",     // system power
        r":\(\)\s*\{.*\};\s*:", // fork bomb
    ])
    .expect("invalid deny patterns")
});

/// Tool that executes shell commands in the workspace.
pub struct ExecTool {
    working_dir: PathBuf,
    timeout: Duration,
}

impl ExecTool {
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self {
            working_dir: working_dir.into(),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        }
    }
}

#[async_trait]
impl Tool for ExecTool {
    fn name(&self) -> &'static str {
        "exec"
    }

    fn description(&self) -> &'static str {
        "Execute a shell command in the workspace"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError> {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("missing 'command' argument".into()))?;

        if has_path_traversal(command) {
            return Err(ToolError::Blocked("path traversal detected".into()));
        }

        if is_blocked(command) {
            return Err(ToolError::Blocked("command matches deny pattern".into()));
        }

        let output = timeout(
            self.timeout,
            Command::new("sh")
                .arg("-c")
                .arg(command)
                .current_dir(&self.working_dir)
                .output(),
        )
        .await
        .map_err(|_| ToolError::Timeout)?
        .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let mut result = format!("$ {command}\n");

        if !stdout.is_empty() {
            result.push_str(&truncate_output(&stdout, MAX_OUTPUT_BYTES));
        }

        if !stderr.is_empty() {
            if !stdout.is_empty() {
                result.push('\n');
            }
            result.push_str("STDERR:\n");
            result.push_str(&truncate_output(&stderr, MAX_OUTPUT_BYTES));
        }

        let _ = write!(
            result,
            "\nExit code: {}",
            output.status.code().unwrap_or(-1)
        );

        Ok(result)
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
        let tool = ExecTool::new(".");
        let args = serde_json::json!({"command": "echo hello"});
        let result = tool.execute(args).await.unwrap();
        assert!(result.contains("hello"));
        assert!(result.contains("Exit code: 0"));
    }

    #[tokio::test]
    async fn test_exec_missing_command() {
        let tool = ExecTool::new(".");
        let args = serde_json::json!({});
        let result = tool.execute(args).await;
        assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
    }

    #[tokio::test]
    async fn test_exec_blocked_command() {
        let tool = ExecTool::new(".");
        let args = serde_json::json!({"command": "rm -rf /"});
        let result = tool.execute(args).await;
        assert!(matches!(result, Err(ToolError::Blocked(_))));
    }

    #[tokio::test]
    async fn test_exec_path_traversal_blocked() {
        let tool = ExecTool::new(".");
        let args = serde_json::json!({"command": "cat ../secret"});
        let result = tool.execute(args).await;
        assert!(matches!(result, Err(ToolError::Blocked(_))));
    }
}
