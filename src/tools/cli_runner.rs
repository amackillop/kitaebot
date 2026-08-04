//! Subprocess execution for CLI tools.
//!
//! [`SubprocessCall`] is a pure value describing what to run.
//! [`exec`] performs the side effect: spawn, wait, collect output.

use std::ffi::OsString;
use std::fmt::Write;
use std::path::PathBuf;
use std::process::Stdio;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::{Duration, timeout};
use tracing::debug;

use crate::error::ToolError;
use crate::sandbox::Tier;

/// Default timeout for subprocess operations.
const TIMEOUT_SECS: u64 = 120;

/// The self-re-exec path for the `confine` wrapper: resolved by the
/// kernel at execve time in the forked child, whose image is still
/// this binary. See the [`crate::confine`] module docs.
pub const CONFINE_SELF: &str = "/proc/self/exe";

/// Landlock confinement for a subprocess: which tier, rooted where.
#[derive(Debug, Clone)]
pub struct Confinement {
    pub tier: Tier,
    pub workspace: PathBuf,
}

// ── Command output ──────────────────────────────────────────────────

/// Raw output from a subprocess.
#[derive(Debug)]
pub struct CmdOutput {
    pub command: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

impl CmdOutput {
    /// Format as `$ command\nstdout\nstderr\nExit code: N`.
    ///
    /// On non-zero exit, returns `ToolError::ExecutionFailed` with the
    /// formatted output so the LLM sees what went wrong.
    pub fn format(&self) -> Result<String, ToolError> {
        let mut result = format!("$ {}\n", self.command);

        if !self.stdout.is_empty() {
            result.push_str(&crate::tools::truncate_output(
                &self.stdout,
                crate::tools::TOOL_OUTPUT_CEILING_BYTES,
            ));
        }
        if !self.stderr.is_empty() {
            if !self.stdout.is_empty() {
                result.push('\n');
            }
            result.push_str(&crate::tools::truncate_output(
                &self.stderr,
                crate::tools::TOOL_OUTPUT_CEILING_BYTES,
            ));
        }

        let _ = write!(result, "\nExit code: {}", self.exit_code);

        if self.exit_code != 0 {
            return Err(ToolError::ExecutionFailed(result));
        }

        Ok(result)
    }
}

// ── Reified subprocess call ─────────────────────────────────────────

/// A description of a subprocess invocation — what to run, not the
/// act of running it. Callers build this value with pure logic;
/// [`exec`] performs the side effect.
#[derive(Debug, Clone)]
pub struct SubprocessCall {
    pub binary: &'static str,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(OsString, OsString)>,
    /// Per-call timeout override. Falls back to [`TIMEOUT_SECS`] when `None`.
    pub timeout_secs: Option<u64>,
    /// Data piped to the subprocess's stdin. `None` leaves stdin closed.
    pub stdin: Option<String>,
    /// Wrap the spawn in a per-child Landlock tier (spec 15). `None`
    /// runs under the daemon's inherited grant — required in unit
    /// tests, where `/proc/self/exe` is the libtest binary.
    pub confine: Option<Confinement>,
}

impl SubprocessCall {
    /// Check whether an environment variable is set.
    #[cfg(test)]
    pub fn has_env(&self, key: &str) -> bool {
        self.env.iter().any(|(k, _)| k == key)
    }
}

/// Execute a [`SubprocessCall`] by spawning a subprocess.
pub async fn exec(call: &SubprocessCall) -> Result<CmdOutput, ToolError> {
    let args_ref: Vec<&str> = call.args.iter().map(String::as_str).collect();
    let mut cmd = if let Some(c) = &call.confine {
        let mut cmd = Command::new(CONFINE_SELF);
        cmd.arg("confine")
            .arg(c.tier.to_string())
            .arg(&c.workspace)
            .arg("--")
            .arg(call.binary)
            .args(&args_ref);
        cmd
    } else {
        let mut cmd = Command::new(call.binary);
        cmd.args(&args_ref);
        cmd
    };
    cmd.current_dir(&call.cwd)
        .env_clear()
        .envs(call.env.iter().map(|(k, v)| (k, v)));
    let label = format!("{} {}", call.binary, args_ref.join(" "));
    let timeout_secs = call.timeout_secs.unwrap_or(TIMEOUT_SECS);
    exec_cmd(&mut cmd, label, timeout_secs, call.stdin.as_deref()).await
}

// ── Command execution ───────────────────────────────────────────────

/// Run a command with timeout and collect output.
async fn exec_cmd(
    cmd: &mut Command,
    command: String,
    timeout_secs: u64,
    stdin: Option<&str>,
) -> Result<CmdOutput, ToolError> {
    debug!(%command, "Running command");

    let output = timeout(Duration::from_secs(timeout_secs), run(cmd, stdin))
        .await
        .map_err(|_| ToolError::Timeout)?
        .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

    Ok(CmdOutput {
        command,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code().unwrap_or(-1),
    })
}

/// Spawn and wait, piping `stdin` into the child when present.
async fn run(cmd: &mut Command, stdin: Option<&str>) -> std::io::Result<std::process::Output> {
    let Some(input) = stdin else {
        return cmd.stdin(Stdio::null()).output().await;
    };
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let mut pipe = child.stdin.take().expect("stdin was piped");
    pipe.write_all(input.as_bytes()).await?;
    drop(pipe);
    child.wait_with_output().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cat_call(stdin: Option<String>) -> SubprocessCall {
        SubprocessCall {
            binary: "cat",
            args: vec![],
            cwd: std::env::temp_dir(),
            env: vec![("PATH".into(), std::env::var_os("PATH").unwrap_or_default())],
            timeout_secs: None,
            stdin,
            confine: None,
        }
    }

    #[tokio::test]
    async fn exec_pipes_stdin_to_child() {
        let out = exec(&cat_call(Some("hello stdin".into()))).await.unwrap();
        assert_eq!(out.stdout, "hello stdin");
        assert_eq!(out.exit_code, 0);
    }

    #[tokio::test]
    async fn exec_without_stdin_closes_it() {
        let out = exec(&cat_call(None)).await.unwrap();
        assert_eq!(out.stdout, "");
        assert_eq!(out.exit_code, 0);
    }
}
