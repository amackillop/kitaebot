//! Subprocess boundary for `gh` and `git` commands.
//!
//! [`GitHubApi`] is the raw trait — it owns credentials and spawns
//! processes. [`RealGitHubApi`] is the production implementation.
//! [`CmdOutput`] captures stdout/stderr/exit code from any subprocess.

use std::fmt::Write;
use std::future::Future;
use std::path::{Path, PathBuf};

use tokio::process::Command;
use tokio::time::{Duration, timeout};
use tracing::debug;

use crate::error::ToolError;
use crate::secrets::Secret;

/// Maximum output bytes before truncation.
const MAX_OUTPUT_BYTES: usize = 10 * 1024;

/// Default timeout for git/gh operations.
const TIMEOUT_SECS: u64 = 120;

// ── Subprocess boundary trait ────────────────────────────────────────

/// Raw subprocess boundary for `gh` and `git` commands.
///
/// Two methods because `gh` (`GH_TOKEN` env) and `git` (`GIT_ASKPASS`
/// lifecycle) have fundamentally different auth mechanisms. Everything
/// above this layer (arg assembly, JSON parsing, formatting) lives in
/// the individual tool modules.
pub trait GitHubApi: Send + Sync {
    /// Run a `gh` CLI command. The token is always injected.
    fn exec_gh(
        &self,
        args: &[&str],
        cwd: &Path,
    ) -> impl Future<Output = Result<CmdOutput, ToolError>> + Send;

    /// Run a `git` command with optional credential injection.
    ///
    /// Only needed for commands that talk to a remote (`clone`, `push`,
    /// `fetch`). Local operations (`commit`, `rev-parse`) pass
    /// `authenticated: false` and skip the `GIT_ASKPASS` lifecycle.
    fn exec_git(
        &self,
        args: &[&str],
        cwd: &Path,
        authenticated: bool,
    ) -> impl Future<Output = Result<CmdOutput, ToolError>> + Send;
}

// ── Real subprocess implementation ──────────────────────────────────

/// Authenticated subprocess executor for `gh` and `git`.
///
/// Owns the token and handles credential injection. For `gh`, the
/// token is passed via `GH_TOKEN` env. For `git`, a temporary
/// `GIT_ASKPASS` script is created and removed after each command.
pub struct RealGitHubApi {
    token: Secret,
}

impl RealGitHubApi {
    pub fn new(token: Secret) -> Self {
        Self { token }
    }
}

impl GitHubApi for RealGitHubApi {
    async fn exec_gh(&self, args: &[&str], cwd: &Path) -> Result<CmdOutput, ToolError> {
        let mut cmd = Command::new("gh");
        cmd.args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(crate::tools::safe_env())
            .env("GH_TOKEN", self.token.expose())
            .env("GH_PROMPT_DISABLED", "1")
            .env("NO_COLOR", "1");
        exec_cmd(&mut cmd, format!("gh {}", args.join(" "))).await
    }

    async fn exec_git(
        &self,
        args: &[&str],
        cwd: &Path,
        authenticated: bool,
    ) -> Result<CmdOutput, ToolError> {
        let mut cmd = Command::new("git");
        cmd.args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(crate::tools::safe_env());

        let askpass = if authenticated {
            let ap = AskPass::create(&self.token).await?;
            cmd.env("GIT_ASKPASS", ap.path())
                .env("GIT_TERMINAL_PROMPT", "0");
            Some(ap)
        } else {
            None
        };

        let output = exec_cmd(&mut cmd, format!("git {}", args.join(" "))).await;
        drop(askpass);
        output
    }
}

// ── Command execution ───────────────────────────────────────────────

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

/// Run a command with timeout and collect output.
///
/// Returns `CmdOutput` on both success and failure — the caller
/// decides how to present it (envelope for the LLM, raw parsing,
/// etc.). Returns `ToolError` only for launch failures and timeouts.
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

// ── GIT_ASKPASS helper ──────────────────────────────────────────────

/// A temporary `GIT_ASKPASS` script that prints the token.
///
/// The script lives in a private temp directory (mode 0700). The
/// directory is owned by a `TempDir` and removed on drop, so cleanup
/// happens even if the git command fails or the future is cancelled.
struct AskPass {
    /// Path to the executable script inside `_dir`.
    path: PathBuf,
    /// Owns the temp directory. Removed on drop.
    _dir: tempfile::TempDir,
}

impl AskPass {
    async fn create(token: &Secret) -> Result<Self, ToolError> {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::Builder::new()
            .prefix("kitaebot-askpass-")
            .tempdir()
            .map_err(|e| ToolError::ExecutionFailed(format!("tmpdir: {e}")))?;

        let path = dir.path().join("askpass");
        let script = format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", token.expose());

        tokio::fs::write(&path, &script)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("write askpass: {e}")))?;

        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("chmod askpass: {e}")))?;

        Ok(Self { path, _dir: dir })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}
