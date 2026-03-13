//! Shared test infrastructure for GitHub tool tests.

use std::path::Path;

use super::GitHub;
use super::api::{CmdOutput, GitHubApi};
use super::client::GitHubClient;
use crate::error::ToolError;

/// Test stub for [`GitHubApi`] that yields pre-enqueued responses.
///
/// Both `exec_gh` and `exec_git` pop from the same queue, so tests
/// enqueue responses in call order regardless of which method fires.
pub struct StubGitHubApi(
    tokio::sync::Mutex<std::collections::VecDeque<Result<CmdOutput, ToolError>>>,
);

impl StubGitHubApi {
    pub fn new(responses: Vec<Result<CmdOutput, ToolError>>) -> Self {
        Self(tokio::sync::Mutex::new(responses.into()))
    }
}

impl GitHubApi for StubGitHubApi {
    async fn exec_gh(&self, _args: &[&str], _cwd: &Path) -> Result<CmdOutput, ToolError> {
        self.0
            .lock()
            .await
            .pop_front()
            .expect("StubGitHubApi: response queue exhausted")
    }

    async fn exec_git(
        &self,
        _args: &[&str],
        _cwd: &Path,
        _authenticated: bool,
    ) -> Result<CmdOutput, ToolError> {
        self.0
            .lock()
            .await
            .pop_front()
            .expect("StubGitHubApi: response queue exhausted")
    }
}

/// Successful `CmdOutput` with the given stdout.
pub fn ok_output(stdout: &str) -> Result<CmdOutput, ToolError> {
    Ok(CmdOutput {
        command: "stub".to_string(),
        stdout: stdout.to_string(),
        stderr: String::new(),
        exit_code: 0,
    })
}

/// Failed `CmdOutput` with the given stderr.
pub fn err_output(stderr: &str) -> Result<CmdOutput, ToolError> {
    Ok(CmdOutput {
        command: "stub".to_string(),
        stdout: String::new(),
        stderr: stderr.to_string(),
        exit_code: 1,
    })
}

/// Build a stub GitHub tool with a fake `.git` dir so `resolve_repo_dir` passes.
pub fn stub_with_repo(
    responses: Vec<Result<CmdOutput, ToolError>>,
) -> (GitHub<StubGitHubApi>, String) {
    let dir = tempfile::tempdir().unwrap();
    let repo = "projects/r";
    std::fs::create_dir_all(dir.path().join(repo).join(".git")).unwrap();
    // Leak the TempDir so it lives for the test duration.
    let path = dir.into_path();
    (
        GitHub::new(StubGitHubApi::new(responses), &path, vec![]),
        repo.to_string(),
    )
}

/// Build a stub GitHubClient with a fake `.git` dir so `resolve_repo_dir` passes.
pub fn stub_client_with_repo(
    responses: Vec<Result<CmdOutput, ToolError>>,
) -> (GitHubClient<StubGitHubApi>, String) {
    let dir = tempfile::tempdir().unwrap();
    let repo = "projects/r";
    std::fs::create_dir_all(dir.path().join(repo).join(".git")).unwrap();
    let path = dir.into_path();
    (
        GitHubClient::new(StubGitHubApi::new(responses), &path, vec![]),
        repo.to_string(),
    )
}
