//! Shared test infrastructure for GitHub tool tests.

use std::ffi::OsString;
use std::path::Path;

use super::client::GitHubClient;
use crate::error::ToolError;
use crate::secrets::Secret;
use crate::tools::cli_runner::{CliRunner, CmdOutput};

/// Test stub for [`CliRunner`] that yields pre-enqueued responses.
///
/// Every `exec` call pops from the same queue, so tests enqueue
/// responses in call order regardless of which binary is invoked.
pub struct StubCliRunner(
    tokio::sync::Mutex<std::collections::VecDeque<Result<CmdOutput, ToolError>>>,
);

impl StubCliRunner {
    pub fn new(responses: Vec<Result<CmdOutput, ToolError>>) -> Self {
        Self(tokio::sync::Mutex::new(responses.into()))
    }
}

impl CliRunner for StubCliRunner {
    async fn exec(
        &self,
        _binary: &str,
        _args: &[&str],
        _cwd: &Path,
        _env: &[(OsString, OsString)],
    ) -> Result<CmdOutput, ToolError> {
        self.0
            .lock()
            .await
            .pop_front()
            .expect("StubCliRunner: response queue exhausted")
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

/// Build a stub GitHubClient with a fake `.git` dir so `resolve_repo_dir` passes.
pub fn stub_client_with_repo(
    responses: Vec<Result<CmdOutput, ToolError>>,
) -> (GitHubClient<StubCliRunner>, String) {
    let dir = tempfile::tempdir().unwrap();
    let repo = "projects/r";
    std::fs::create_dir_all(dir.path().join(repo).join(".git")).unwrap();
    let path = dir.into_path();
    (
        GitHubClient::new(
            StubCliRunner::new(responses),
            Secret::test("fake"),
            &path,
            vec![],
        ),
        repo.to_string(),
    )
}

/// Build a stub `Arc<GitHubClient>` with a fake `.git` dir.
///
/// Used by individual tool tests that hold `Arc<GitHubClient<R>>`.
pub fn stub_arc_with_repo(
    responses: Vec<Result<CmdOutput, ToolError>>,
) -> (std::sync::Arc<GitHubClient<StubCliRunner>>, String) {
    let (client, repo) = stub_client_with_repo(responses);
    (std::sync::Arc::new(client), repo)
}
