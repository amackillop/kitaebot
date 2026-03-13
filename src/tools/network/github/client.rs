//! Shared client context for GitHub tools.
//!
//! [`GitHubClient`] wraps a [`GitHubApi`] impl and carries workspace
//! root + co-author config. It provides the plumbing methods that all
//! tool modules use: subprocess execution, JSON parsing, repo dir
//! validation, and branch detection.

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;

use super::api::GitHubApi;
use crate::error::ToolError;

/// Shared context for all GitHub tools.
///
/// Generic over [`GitHubApi`] so tests can substitute a stub without
/// spawning real subprocesses.
pub struct GitHubClient<A> {
    pub(super) api: A,
    pub(super) workspace_root: PathBuf,
    pub(super) co_authors: Vec<String>,
}

impl<A: GitHubApi> GitHubClient<A> {
    pub fn new(api: A, workspace_root: impl Into<PathBuf>, co_authors: Vec<String>) -> Self {
        Self {
            api,
            workspace_root: workspace_root.into(),
            co_authors,
        }
    }

    /// Resolve and validate a repo directory within the workspace.
    pub fn resolve_repo_dir(&self, repo_dir: &str) -> Result<PathBuf, ToolError> {
        if repo_dir.contains("..") {
            return Err(ToolError::Blocked(
                "repo_dir: path traversal detected".into(),
            ));
        }
        if Path::new(repo_dir).is_absolute() {
            return Err(ToolError::Blocked(
                "repo_dir: absolute paths not allowed".into(),
            ));
        }

        let resolved = self.workspace_root.join(repo_dir);
        if !resolved.starts_with(&self.workspace_root) {
            return Err(ToolError::Blocked("repo_dir: escapes workspace".into()));
        }
        if !resolved.join(".git").is_dir() {
            return Err(ToolError::InvalidArguments(format!(
                "{repo_dir} is not a git repository"
            )));
        }

        Ok(resolved)
    }

    /// Get the current branch name from a git working directory.
    pub async fn current_branch(&self, cwd: &Path) -> Result<String, ToolError> {
        let output = self
            .api
            .exec_git(&["rev-parse", "--abbrev-ref", "HEAD"], cwd, false)
            .await?;
        if output.exit_code != 0 {
            return Err(ToolError::ExecutionFailed(format!(
                "failed to get current branch: {}",
                output.stderr
            )));
        }
        Ok(output.stdout.trim().to_string())
    }

    /// Run a git command, format output as envelope for the LLM.
    pub async fn run_git(
        &self,
        args: &[&str],
        cwd: &Path,
        authenticated: bool,
    ) -> Result<String, ToolError> {
        self.api.exec_git(args, cwd, authenticated).await?.format()
    }

    /// Run a `gh` command, format output as envelope for the LLM.
    pub async fn run_gh(&self, args: &[&str], cwd: &Path) -> Result<String, ToolError> {
        self.api.exec_gh(args, cwd).await?.format()
    }

    /// Run `gh` with `--json <fields>` and deserialize the response.
    ///
    /// Appends `--json <fields>` to `args` automatically, so callers
    /// specify only the subcommand flags.
    pub async fn run_gh_json<T: DeserializeOwned>(
        &self,
        args: &[&str],
        fields: &str,
        cwd: &Path,
    ) -> Result<T, ToolError> {
        let full_args: Vec<&str> = args.iter().copied().chain(["--json", fields]).collect();
        self.run_gh_parse(&full_args, cwd).await
    }

    /// Run `gh api` and deserialize the JSON response.
    pub async fn run_gh_api<T: DeserializeOwned>(
        &self,
        endpoint: &str,
        cwd: &Path,
    ) -> Result<T, ToolError> {
        self.run_gh_parse(&["api", endpoint], cwd).await
    }

    /// Run `gh`, check exit code, and deserialize stdout as JSON.
    pub async fn run_gh_parse<T: DeserializeOwned>(
        &self,
        args: &[&str],
        cwd: &Path,
    ) -> Result<T, ToolError> {
        let output = self.api.exec_gh(args, cwd).await?;
        if output.exit_code != 0 {
            return Err(ToolError::ExecutionFailed(format!(
                "{}: {}",
                output.command, output.stderr
            )));
        }
        serde_json::from_str(&output.stdout)
            .map_err(|e| ToolError::ExecutionFailed(format!("{}: {e}", output.command)))
    }
}

#[cfg(test)]
mod tests {
    use super::super::api::CmdOutput;
    use super::*;
    use crate::error::ToolError;

    // ── Repo dir validation ─────────────────────────────────────────

    #[test]
    fn resolve_repo_dir_rejects_traversal() {
        let client = make_client(tempfile::tempdir().unwrap().path());
        assert!(matches!(
            client.resolve_repo_dir("../escape"),
            Err(ToolError::Blocked(_))
        ));
    }

    #[test]
    fn resolve_repo_dir_rejects_absolute() {
        let client = make_client(tempfile::tempdir().unwrap().path());
        assert!(matches!(
            client.resolve_repo_dir("/etc"),
            Err(ToolError::Blocked(_))
        ));
    }

    #[test]
    fn resolve_repo_dir_rejects_non_repo() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("projects/notrepo")).unwrap();
        let client = make_client(dir.path());
        assert!(matches!(
            client.resolve_repo_dir("projects/notrepo"),
            Err(ToolError::InvalidArguments(_))
        ));
    }

    #[test]
    fn resolve_repo_dir_accepts_valid_repo() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("projects/myrepo/.git")).unwrap();
        let client = make_client(dir.path());
        let resolved = client.resolve_repo_dir("projects/myrepo").unwrap();
        assert!(resolved.ends_with("projects/myrepo"));
    }

    // ── run_gh / run_gh_parse ────────────────────────────────────────

    #[tokio::test]
    async fn run_gh_nonzero_exit_returns_error() {
        let (client, repo) = stub_with_repo(vec![err_output("not found")]);
        let cwd = client.resolve_repo_dir(&repo).unwrap();
        let result = client.run_gh(&["pr", "view"], &cwd).await;
        assert!(matches!(result, Err(ToolError::ExecutionFailed(_))));
    }

    #[tokio::test]
    async fn run_gh_parse_malformed_json_returns_error() {
        let (client, repo) = stub_with_repo(vec![ok_output("not json")]);
        let cwd = client.resolve_repo_dir(&repo).unwrap();
        let result: Result<Vec<serde_json::Value>, _> =
            client.run_gh_parse(&["pr", "list"], &cwd).await;
        assert!(matches!(result, Err(ToolError::ExecutionFailed(_))));
    }

    #[tokio::test]
    async fn run_gh_parse_nonzero_exit_returns_stderr() {
        let (client, repo) = stub_with_repo(vec![err_output("permission denied")]);
        let cwd = client.resolve_repo_dir(&repo).unwrap();
        let result: Result<Vec<serde_json::Value>, _> =
            client.run_gh_parse(&["pr", "list"], &cwd).await;
        assert!(
            matches!(result, Err(ToolError::ExecutionFailed(msg)) if msg.contains("permission denied"))
        );
    }

    // ── current_branch ───────────────────────────────────────────────

    #[tokio::test]
    async fn current_branch_trims_output() {
        let (client, repo) = stub_with_repo(vec![ok_output("  feature/foo\n")]);
        let cwd = client.resolve_repo_dir(&repo).unwrap();
        let branch = client.current_branch(&cwd).await.unwrap();
        assert_eq!(branch, "feature/foo");
    }

    #[tokio::test]
    async fn current_branch_nonzero_exit() {
        let (client, repo) = stub_with_repo(vec![err_output("not a git repo")]);
        let cwd = client.resolve_repo_dir(&repo).unwrap();
        let result = client.current_branch(&cwd).await;
        assert!(
            matches!(result, Err(ToolError::ExecutionFailed(msg)) if msg.contains("current branch"))
        );
    }

    // ── Test helpers ────────────────────────────────────────────────

    struct StubGitHubApi(
        tokio::sync::Mutex<std::collections::VecDeque<Result<CmdOutput, ToolError>>>,
    );

    impl StubGitHubApi {
        fn new(responses: Vec<Result<CmdOutput, ToolError>>) -> Self {
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

    fn ok_output(stdout: &str) -> Result<CmdOutput, ToolError> {
        Ok(CmdOutput {
            command: "stub".to_string(),
            stdout: stdout.to_string(),
            stderr: String::new(),
            exit_code: 0,
        })
    }

    fn err_output(stderr: &str) -> Result<CmdOutput, ToolError> {
        Ok(CmdOutput {
            command: "stub".to_string(),
            stdout: String::new(),
            stderr: stderr.to_string(),
            exit_code: 1,
        })
    }

    fn make_client(workspace: &std::path::Path) -> GitHubClient<StubGitHubApi> {
        GitHubClient::new(StubGitHubApi::new(vec![]), workspace, vec![])
    }

    fn stub_with_repo(
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
}
