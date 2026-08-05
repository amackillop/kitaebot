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

// ── Process-group kill guard ────────────────────────────────────────

/// Kills the child's process group when dropped, unless disarmed.
///
/// Children are spawned as group leaders, so the sweep takes every
/// descendant — grandchildren `kill_on_drop` cannot reach. A normal
/// exit disarms: what a finished command deliberately left running in
/// the background is its own business; a timeout or a cancelled turn
/// means the turn lost control of the tree. Descendants that call
/// `setsid` escape the group and the sweep.
pub struct GroupKillGuard(Option<i32>);

impl GroupKillGuard {
    /// Arm for `child`'s group. The child must have been spawned with
    /// `process_group(0)`, making its pid the pgid.
    pub fn arm(child: &tokio::process::Child) -> Self {
        Self(child.id().and_then(|pid| i32::try_from(pid).ok()))
    }

    /// The command completed; leave its group alone.
    pub fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for GroupKillGuard {
    fn drop(&mut self) {
        if let Some(pgid) = self.0 {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pgid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
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
    // The argv as actually spawned — confine wrapper included, so an
    // error or log line shows exactly what the kernel was asked to run,
    // not the logical command it stands for.
    let argv = confined_argv(call, &args_ref);
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
    // The logical command, for the returned CmdOutput's `$ …` echo.
    let label = format!("{} {}", call.binary, args_ref.join(" "));
    let timeout_secs = call.timeout_secs.unwrap_or(TIMEOUT_SECS);
    exec_cmd(
        &mut cmd,
        label,
        &argv,
        &call.cwd,
        timeout_secs,
        call.stdin.as_deref(),
    )
    .await
}

/// Render the argv as spawned, with the `confine` wrapper prefix when
/// the call is confined, for error messages and the debug log.
fn confined_argv(call: &SubprocessCall, args: &[&str]) -> String {
    let tail = format!("{} {}", call.binary, args.join(" "));
    match &call.confine {
        Some(c) => format!(
            "{CONFINE_SELF} confine {} {} -- {tail}",
            c.tier,
            c.workspace.display(),
        ),
        None => tail,
    }
}

// ── Command execution ───────────────────────────────────────────────

/// Run a command with timeout and collect output.
///
/// `argv` is the real spawned argument vector (confine wrapper and
/// all); `label` is the logical command echoed back in the output.
async fn exec_cmd(
    cmd: &mut Command,
    label: String,
    argv: &str,
    cwd: &std::path::Path,
    timeout_secs: u64,
    stdin: Option<&str>,
) -> Result<CmdOutput, ToolError> {
    debug!(argv, cwd = %cwd.display(), "spawning");

    let output = timeout(Duration::from_secs(timeout_secs), run(cmd, stdin))
        .await
        .map_err(|_| ToolError::Timeout {
            command: argv.to_string(),
            secs: timeout_secs,
        })?
        .map_err(|source| ToolError::Spawn {
            argv: argv.to_string(),
            cwd: cwd.display().to_string(),
            source,
        })?;

    Ok(CmdOutput {
        command: label,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code().unwrap_or(-1),
    })
}

/// Spawn and wait, piping `stdin` into the child when present.
///
/// The child leads its own process group, and the group is swept on
/// timeout or cancellation (see [`GroupKillGuard`]) — a dropped wait
/// future must not leave a `git fetch` or a warm build running.
async fn run(cmd: &mut Command, stdin: Option<&str>) -> std::io::Result<std::process::Output> {
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    match stdin {
        Some(_) => cmd.stdin(Stdio::piped()),
        None => cmd.stdin(Stdio::null()),
    };
    let mut child = cmd.spawn()?;
    let guard = GroupKillGuard::arm(&child);
    if let Some(input) = stdin {
        let mut pipe = child.stdin.take().expect("stdin was piped");
        pipe.write_all(input.as_bytes()).await?;
        drop(pipe);
    }
    let output = child.wait_with_output().await?;
    guard.disarm();
    Ok(output)
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

    #[test]
    fn confined_argv_shows_the_wrapper() {
        let mut call = cat_call(None);
        call.binary = "git";
        call.args = vec!["ls-remote".into(), "url".into()];
        let bare = confined_argv(&call, &["ls-remote", "url"]);
        assert_eq!(bare, "git ls-remote url");

        call.confine = Some(Confinement {
            tier: Tier::Git,
            workspace: PathBuf::from("/ws"),
        });
        let wrapped = confined_argv(&call, &["ls-remote", "url"]);
        assert_eq!(
            wrapped,
            "/proc/self/exe confine git /ws -- git ls-remote url"
        );
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

    fn bash_call(dir: &std::path::Path, script: &str, timeout_secs: u64) -> SubprocessCall {
        SubprocessCall {
            binary: "bash",
            args: vec!["-c".into(), script.into()],
            cwd: dir.to_path_buf(),
            env: vec![("PATH".into(), std::env::var_os("PATH").unwrap_or_default())],
            timeout_secs: Some(timeout_secs),
            stdin: None,
            confine: None,
        }
    }

    #[tokio::test]
    async fn timeout_kills_the_whole_group() {
        let dir = tempfile::tempdir().unwrap();
        // The backgrounded subshell outlives the direct bash child;
        // only the group sweep can stop it touching the marker at ~2s.
        let call = bash_call(dir.path(), "(sleep 2 && touch marker) & sleep 5", 1);

        let err = exec(&call).await.unwrap_err();

        assert!(matches!(err, ToolError::Timeout { .. }));
        tokio::time::sleep(Duration::from_millis(1300)).await;
        assert!(!dir.path().join("marker").exists());
    }

    #[tokio::test]
    async fn normal_exit_leaves_background_children_alone() {
        let dir = tempfile::tempdir().unwrap();
        // Deliberate backgrounding is the command's business: the guard
        // disarms on a normal exit and the grandchild survives.
        let call = bash_call(dir.path(), "(sleep 0.2 && touch marker) &", 5);

        let out = exec(&call).await.unwrap();

        assert_eq!(out.exit_code, 0);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(dir.path().join("marker").exists());
    }
}
