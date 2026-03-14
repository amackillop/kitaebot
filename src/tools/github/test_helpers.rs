//! Shared test infrastructure for GitHub tool tests.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::Path;
use std::string::ToString;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::gh_cli::GhCli;
use super::git_cli::GitCli;
use crate::error::ToolError;
use crate::secrets::Secret;
use crate::tools::cli_runner::{CliRunner, CmdOutput};

// ── Recorded call ────────────────────────────────────────────────────

/// A single subprocess invocation captured by [`StubCliRunner`].
#[derive(Debug, Clone)]
pub struct RecordedCall {
    pub binary: String,
    pub args: Vec<String>,
    pub env_keys: Vec<String>,
}

impl RecordedCall {
    /// Check whether an environment variable was set.
    pub fn has_env(&self, key: &str) -> bool {
        self.env_keys.iter().any(|k| k == key)
    }
}

// ── Calls handle ─────────────────────────────────────────────────────

/// Shared handle to the recorded calls list.
///
/// Cloned from [`StubCliRunner`] at construction time so tests can
/// inspect calls after the runner has been moved into a `GitCli`/`GhCli`.
#[derive(Clone, Default)]
pub struct Calls(Arc<Mutex<Vec<RecordedCall>>>);

impl Calls {
    /// Drain and return all recorded calls.
    pub async fn take(&self) -> Vec<RecordedCall> {
        std::mem::take(&mut *self.0.lock().await)
    }
}

// ── Stub runner ──────────────────────────────────────────────────────

/// Test stub for [`CliRunner`] that yields pre-enqueued responses
/// and records every invocation for later assertion.
///
/// Every `exec` call pops from the same queue, so tests enqueue
/// responses in call order regardless of which binary is invoked.
pub struct StubCliRunner {
    responses: Mutex<VecDeque<Result<CmdOutput, ToolError>>>,
    calls: Calls,
}

impl StubCliRunner {
    pub fn new(responses: Vec<Result<CmdOutput, ToolError>>) -> (Self, Calls) {
        let calls = Calls::default();
        let runner = Self {
            responses: Mutex::new(responses.into()),
            calls: calls.clone(),
        };
        (runner, calls)
    }
}

impl CliRunner for StubCliRunner {
    async fn exec(
        &self,
        binary: &str,
        args: &[&str],
        _cwd: &Path,
        env: &[(OsString, OsString)],
    ) -> Result<CmdOutput, ToolError> {
        self.calls.0.lock().await.push(RecordedCall {
            binary: binary.to_string(),
            args: args.iter().map(ToString::to_string).collect(),
            env_keys: env
                .iter()
                .map(|(k, _)| k.to_string_lossy().into_owned())
                .collect(),
        });
        self.responses
            .lock()
            .await
            .pop_front()
            .expect("StubCliRunner: response queue exhausted")
    }
}

// ── Helper constructors ──────────────────────────────────────────────

/// Successful `CmdOutput` with the given stdout.
#[allow(clippy::unnecessary_wraps)]
pub fn ok_output(stdout: &str) -> Result<CmdOutput, ToolError> {
    Ok(CmdOutput {
        command: "stub".to_string(),
        stdout: stdout.to_string(),
        stderr: String::new(),
        exit_code: 0,
    })
}

/// Failed `CmdOutput` with the given stderr.
#[allow(clippy::unnecessary_wraps)]
pub fn err_output(stderr: &str) -> Result<CmdOutput, ToolError> {
    Ok(CmdOutput {
        command: "stub".to_string(),
        stdout: String::new(),
        stderr: stderr.to_string(),
        exit_code: 1,
    })
}

/// Build a stub `GitCli` with a fake `.git` dir.
#[allow(deprecated)] // tempfile::TempDir::into_path
pub fn stub_git_cli_with_repo(
    responses: Vec<Result<CmdOutput, ToolError>>,
) -> (GitCli<StubCliRunner>, String, Calls) {
    let dir = tempfile::tempdir().unwrap();
    let repo = "projects/r";
    std::fs::create_dir_all(dir.path().join(repo).join(".git")).unwrap();
    let path = dir.into_path();
    let (runner, calls) = StubCliRunner::new(responses);
    (
        GitCli::new(runner, Secret::test("fake"), &path, vec![]),
        repo.to_string(),
        calls,
    )
}

/// Build a stub `Arc<GitCli>` with a fake `.git` dir.
pub fn stub_git_arc_with_repo(
    responses: Vec<Result<CmdOutput, ToolError>>,
) -> (Arc<GitCli<StubCliRunner>>, String, Calls) {
    let (cli, repo, calls) = stub_git_cli_with_repo(responses);
    (Arc::new(cli), repo, calls)
}

/// Build a stub `GhCli` with a fake `.git` dir.
#[allow(deprecated)] // tempfile::TempDir::into_path
pub fn stub_gh_cli_with_repo(
    responses: Vec<Result<CmdOutput, ToolError>>,
) -> (GhCli<StubCliRunner>, String, Calls) {
    let dir = tempfile::tempdir().unwrap();
    let repo = "projects/r";
    std::fs::create_dir_all(dir.path().join(repo).join(".git")).unwrap();
    let path = dir.into_path();
    let (runner, calls) = StubCliRunner::new(responses);
    (
        GhCli::new(runner, Secret::test("fake"), &path),
        repo.to_string(),
        calls,
    )
}

/// Build a stub `Arc<GhCli>` with a fake `.git` dir.
pub fn stub_gh_arc_with_repo(
    responses: Vec<Result<CmdOutput, ToolError>>,
) -> (Arc<GhCli<StubCliRunner>>, String, Calls) {
    let (cli, repo, calls) = stub_gh_cli_with_repo(responses);
    (Arc::new(cli), repo, calls)
}
