//! Generic subprocess boundary for CLI tools.
//!
//! [`CliRunner`] is the raw trait — a single `exec` method that spawns
//! a subprocess with an explicit binary, args, cwd, and environment.
//! Auth-agnostic: callers build the full env (`safe_env` + credentials)
//! and pass it in.
//!
//! [`RealCliRunner`] is the production implementation: a unit struct
//! that spawns via `tokio::process::Command` with `env_clear().envs(env)`.

use std::ffi::OsString;
use std::fmt::Write;
use std::future::Future;
use std::path::{Path, PathBuf};

use tokio::process::Command;
use tokio::time::{Duration, timeout};
use tracing::debug;

use crate::error::ToolError;

/// Maximum output bytes before truncation.
pub(crate) const MAX_OUTPUT_BYTES: usize = 10 * 1024;

/// Default timeout for subprocess operations.
const TIMEOUT_SECS: u64 = 120;

// ── Subprocess boundary trait ────────────────────────────────────────

/// Generic subprocess boundary.
///
/// A single method that spawns any binary with explicit env. Callers
/// are responsible for building the full environment (`safe_env` +
/// credentials).
#[allow(dead_code)] // removed in the next commit
pub(crate) trait CliRunner: Send + Sync {
    fn exec(
        &self,
        binary: &str,
        args: &[&str],
        cwd: &Path,
        env: &[(OsString, OsString)],
    ) -> impl Future<Output = Result<CmdOutput, ToolError>> + Send;
}

// ── Real subprocess implementation ──────────────────────────────────

/// Production subprocess executor. Spawns the process with
/// `env_clear().envs(env)`.
#[allow(dead_code)] // removed in the next commit
#[derive(Clone, Copy)]
pub(crate) struct RealCliRunner;

impl CliRunner for RealCliRunner {
    async fn exec(
        &self,
        binary: &str,
        args: &[&str],
        cwd: &Path,
        env: &[(OsString, OsString)],
    ) -> Result<CmdOutput, ToolError> {
        let mut cmd = Command::new(binary);
        cmd.args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k, v)));
        exec_cmd(&mut cmd, format!("{binary} {}", args.join(" "))).await
    }
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
                MAX_OUTPUT_BYTES,
            ));
        }
        if !self.stderr.is_empty() {
            if !self.stdout.is_empty() {
                result.push('\n');
            }
            result.push_str(&crate::tools::truncate_output(
                &self.stderr,
                MAX_OUTPUT_BYTES,
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
    let mut cmd = Command::new(call.binary);
    cmd.args(&args_ref)
        .current_dir(&call.cwd)
        .env_clear()
        .envs(call.env.iter().map(|(k, v)| (k, v)));
    let label = format!("{} {}", call.binary, args_ref.join(" "));
    exec_cmd(&mut cmd, label).await
}

// ── Command execution ───────────────────────────────────────────────

/// Run a command with timeout and collect output.
async fn exec_cmd(cmd: &mut Command, command: String) -> Result<CmdOutput, ToolError> {
    debug!(%command, "Running command");

    let output = timeout(Duration::from_secs(TIMEOUT_SECS), cmd.output())
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
