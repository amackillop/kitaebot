//! Subprocess execution for CLI tools.
//!
//! [`SubprocessCall`] is a pure value describing what to run.
//! [`exec`] performs the side effect: spawn, wait, collect output.

use std::ffi::OsString;
use std::fmt::Write;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::{Duration, timeout};
use tracing::debug;

use crate::error::{TimeoutEvidence, ToolError};
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
    /// On non-zero exit, returns `ToolError::CommandFailed` carrying
    /// that same text, so the LLM sees what went wrong.
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
            return Err(ToolError::CommandFailed {
                command: self.command.clone(),
                exit_code: self.exit_code,
                output: result,
            });
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

    let spawn_err = |source| ToolError::Spawn {
        argv: argv.to_string(),
        cwd: cwd.display().to_string(),
        source,
    };
    let child = spawn_child(cmd, stdin).await.map_err(spawn_err)?;
    let output = wait_with_evidence(child, Duration::from_secs(timeout_secs))
        .await
        .map_err(spawn_err)?
        .map_err(|evidence| ToolError::Timeout {
            command: argv.to_string(),
            secs: timeout_secs,
            evidence,
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
async fn spawn_child(
    cmd: &mut Command,
    stdin: Option<&str>,
) -> std::io::Result<tokio::process::Child> {
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    match stdin {
        Some(_) => cmd.stdin(Stdio::piped()),
        None => cmd.stdin(Stdio::null()),
    };
    // ETXTBSY: a concurrent fork inherited a still-open write fd to
    // this binary and has not exec'd yet (O_CLOEXEC closes it only at
    // exec). The window is microseconds; a bounded retry is the
    // standard remedy (cf. cargo). process_group(0) forces fork+exec
    // over posix_spawn, which is what opens the window here.
    let mut attempts = 0;
    let mut child = loop {
        match cmd.spawn() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && attempts < 3 => {
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            result => break result?,
        }
    };
    if let Some(input) = stdin {
        let mut pipe = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("child stdin not piped despite stdin input"))?;
        pipe.write_all(input.as_bytes()).await?;
        drop(pipe);
    }
    Ok(child)
}

/// Incremental pipe reader whose buffer survives a timeout kill.
struct PipeReader {
    buf: Arc<Mutex<Vec<u8>>>,
    task: tokio::task::JoinHandle<()>,
}

/// Post-kill grace for readers to drain what the pipe still holds.
const READER_GRACE: Duration = Duration::from_secs(2);

impl PipeReader {
    fn spawn(pipe: Option<impl AsyncReadExt + Unpin + Send + 'static>) -> Self {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&buf);
        let task = tokio::spawn(async move {
            let Some(mut pipe) = pipe else { return };
            let mut chunk = [0u8; 8192];
            loop {
                match pipe.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sink
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .extend(&chunk[..n]),
                }
            }
        });
        Self { buf, task }
    }

    /// Wait briefly for EOF (the kill closes the pipe), then take
    /// whatever arrived — partial output beats none.
    async fn finish(self) -> Vec<u8> {
        let _ = timeout(READER_GRACE, self.task).await;
        std::mem::take(
            &mut self
                .buf
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

/// Wait for the child within `budget`, keeping its output readable
/// even when the budget kills it. On timeout the group is swept and
/// [`TimeoutEvidence`] is returned so the error can say what the
/// child was doing when it died and under what pressure (#74's stall
/// class is silent memory.high throttling that no kernel log names).
pub(crate) async fn wait_with_evidence(
    mut child: tokio::process::Child,
    budget: Duration,
) -> std::io::Result<Result<std::process::Output, TimeoutEvidence>> {
    let guard = GroupKillGuard::arm(&child);
    let stdout = PipeReader::spawn(child.stdout.take());
    let stderr = PipeReader::spawn(child.stderr.take());
    match timeout(budget, child.wait()).await {
        Ok(status) => {
            guard.disarm();
            let status = status?;
            Ok(Ok(std::process::Output {
                status,
                stdout: stdout.finish().await,
                stderr: stderr.finish().await,
            }))
        }
        Err(_elapsed) => {
            // Sweep the group first so the pipes close and the
            // readers reach EOF instead of the grace timeout.
            drop(guard);
            let pressure = cgroup_snapshot();
            let out = String::from_utf8_lossy(&stdout.finish().await).into_owned();
            let err = String::from_utf8_lossy(&stderr.finish().await).into_owned();
            Ok(Err(TimeoutEvidence {
                output_tail: format_output_tail(&out, &err),
                pressure,
            }))
        }
    }
}

/// Bytes of stderr kept in timeout evidence; stderr first and larger
/// because build tools put diagnoses there ("Blocking waiting for
/// file lock ...").
const EVIDENCE_STDERR_BYTES: usize = 1_200;
const EVIDENCE_STDOUT_BYTES: usize = 600;

/// Combined bounded tail of a killed child's streams. Pure.
fn format_output_tail(stdout: &str, stderr: &str) -> String {
    let mut tail = String::new();
    if !stderr.trim().is_empty() {
        let _ = write!(
            tail,
            "stderr: {}",
            super::truncate_head(stderr.trim_end(), EVIDENCE_STDERR_BYTES)
        );
    }
    if !stdout.trim().is_empty() {
        if !tail.is_empty() {
            tail.push('\n');
        }
        let _ = write!(
            tail,
            "stdout: {}",
            super::truncate_head(stdout.trim_end(), EVIDENCE_STDOUT_BYTES)
        );
    }
    tail
}

/// One-line pressure snapshot of the daemon's own cgroup — exec
/// children share it, which is the suspected stall mechanism (#74).
/// Best-effort: a snapshot that cannot be read reports why instead of
/// masking the timeout.
fn cgroup_snapshot() -> String {
    let cgroup = match std::fs::read_to_string("/proc/self/cgroup") {
        Ok(s) => s,
        Err(e) => return format!("(unavailable: /proc/self/cgroup: {e})"),
    };
    let Some(rel) = parse_cgroup_v2_path(&cgroup) else {
        return "(unavailable: no cgroup v2 entry)".into();
    };
    let base = format!("/sys/fs/cgroup{rel}");
    let read = |file: &str| std::fs::read_to_string(format!("{base}/{file}")).ok();
    format_snapshot(
        read("memory.current").as_deref(),
        read("memory.events").as_deref(),
        read("memory.pressure").as_deref(),
        read("cpu.pressure").as_deref(),
    )
}

/// The v2 path from `/proc/self/cgroup` (`0::/system.slice/...`). Pure.
fn parse_cgroup_v2_path(contents: &str) -> Option<&str> {
    contents
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(str::trim)
}

/// Render the snapshot files into one journal-greppable line. Pure.
fn format_snapshot(
    current: Option<&str>,
    events: Option<&str>,
    mem_psi: Option<&str>,
    cpu_psi: Option<&str>,
) -> String {
    let mut parts = Vec::new();
    if let Some(bytes) = current.and_then(|c| c.trim().parse::<u64>().ok()) {
        // Integer MiB: precise enough for a pressure snapshot, and
        // u64 -> f64 is a clippy hard error here.
        parts.push(format!("memory.current={}M", bytes / (1024 * 1024)));
    }
    if let Some(events) = events {
        for counter in ["high", "max", "oom"] {
            if let Some(v) = events
                .lines()
                .find_map(|l| l.strip_prefix(counter).map(str::trim))
            {
                parts.push(format!("memory.events.{counter}={v}"));
            }
        }
    }
    for (name, psi) in [("memory.psi", mem_psi), ("cpu.psi", cpu_psi)] {
        if let Some(some) = psi.and_then(|p| p.lines().find(|l| l.starts_with("some"))) {
            parts.push(format!("{name} {}", some.trim()));
        }
    }
    if parts.is_empty() {
        "(unavailable: no readable cgroup files)".into()
    } else {
        parts.join(", ")
    }
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
    async fn timeout_error_carries_the_childs_last_words() {
        let call = SubprocessCall {
            binary: "bash",
            args: vec![
                "-c".into(),
                "echo lock-marker >&2; echo out-marker; sleep 30".into(),
            ],
            cwd: std::env::temp_dir(),
            env: vec![("PATH".into(), std::env::var_os("PATH").unwrap_or_default())],
            timeout_secs: Some(1),
            stdin: None,
            confine: None,
        };
        let err = exec(&call).await.unwrap_err();
        let ToolError::Timeout { secs, evidence, .. } = err else {
            panic!("expected Timeout, got {err:?}");
        };
        assert_eq!(secs, 1);
        assert!(evidence.output_tail.contains("lock-marker"), "{evidence}");
        assert!(evidence.output_tail.contains("out-marker"), "{evidence}");
        // The snapshot is best-effort but never empty: real values or
        // a reason.
        assert!(!evidence.pressure.is_empty());
    }

    #[test]
    fn parses_cgroup_v2_path() {
        assert_eq!(
            parse_cgroup_v2_path("0::/system.slice/kitaebot.service\n"),
            Some("/system.slice/kitaebot.service")
        );
        assert_eq!(parse_cgroup_v2_path("1:name=systemd:/\n"), None);
    }

    #[test]
    fn formats_snapshot_from_cgroup_files() {
        let s = format_snapshot(
            Some("4404019200\n"),
            Some("low 0\nhigh 1234\nmax 5\noom 0\noom_kill 0\n"),
            Some("some avg10=42.10 avg60=38.50 avg300=12.00 total=99\nfull avg10=1.0\n"),
            Some("some avg10=88.00 avg60=70.00 avg300=30.00 total=11\n"),
        );
        assert!(s.contains("memory.current=4200M"), "{s}");
        assert!(s.contains("memory.events.high=1234"), "{s}");
        assert!(s.contains("memory.psi some avg10=42.10"), "{s}");
        assert!(s.contains("cpu.psi some avg10=88.00"), "{s}");
        assert_eq!(
            format_snapshot(None, None, None, None),
            "(unavailable: no readable cgroup files)"
        );
    }

    #[test]
    fn output_tail_prefers_stderr_and_bounds_both() {
        let tail = format_output_tail(&"o".repeat(5000), &"e".repeat(5000));
        assert!(tail.starts_with("stderr:"), "{tail}");
        assert!(tail.contains("stdout:"));
        assert!(tail.len() < 2 * (EVIDENCE_STDERR_BYTES + EVIDENCE_STDOUT_BYTES));
        assert_eq!(format_output_tail("", "  \n"), "");
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
