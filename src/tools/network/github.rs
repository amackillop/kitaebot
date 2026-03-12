//! GitHub integration tool.
//!
//! Provides authenticated git and GitHub CLI operations. The token never
//! reaches the exec tool — it is injected only into subprocesses spawned
//! by this module via `GIT_ASKPASS` (for git) or `GH_TOKEN` (for `gh`).
//!
//! # Token injection
//!
//! For `git clone`/`push`, a temporary helper script is written to a
//! private directory, set as `GIT_ASKPASS`, and deleted immediately after
//! the subprocess exits. The script prints the token to stdout when
//! invoked by git. The token is on disk for the duration of one git
//! command only.

use std::fmt::Write;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::Deserialize;
use tokio::process::Command;
use tokio::time::{Duration, timeout};
use tracing::debug;

use std::future::Future;
use std::pin::Pin;

use super::Tool;
use crate::error::ToolError;
use crate::secrets::Secret;

/// Maximum output bytes before truncation.
const MAX_OUTPUT_BYTES: usize = 10 * 1024;

/// Default timeout for git/gh operations.
const TIMEOUT_SECS: u64 = 120;

/// Arguments for the GitHub tool.
///
/// Each variant maps to one git/gh subcommand. Tagged with `action`
/// so the LLM produces `{"action": "clone", "url": "..."}`.
#[derive(Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Args {
    /// Fetch the latest failed CI run and its failure logs.
    CiStatus {
        /// Repository directory relative to workspace root
        /// (e.g. `"projects/myrepo"`).
        repo_dir: String,
        /// Branch to check. Defaults to the currently checked-out branch.
        branch: Option<String>,
    },
    /// Clone a repository into the workspace.
    Clone {
        /// Repository URL (HTTPS or SSH). SSH URLs are rewritten to HTTPS
        /// automatically.
        url: String,
        /// Target directory name inside `projects/`. Defaults to the
        /// repository name derived from the URL.
        name: Option<String>,
    },
    /// Push commits to a remote.
    Push {
        /// Repository directory relative to workspace root
        /// (e.g. `"projects/myrepo"`).
        repo_dir: String,
        /// Remote name. Defaults to `"origin"`.
        remote: Option<String>,
        /// Branch to push. Defaults to the current branch.
        branch: Option<String>,
        /// Set upstream tracking (`--set-upstream`).
        #[serde(default)]
        set_upstream: bool,
    },
    /// Create a pull request.
    PrCreate {
        /// Repository directory relative to workspace root.
        repo_dir: String,
        /// PR title.
        title: String,
        /// PR body / description.
        body: String,
        /// Base branch to merge into. Defaults to the repo's default branch.
        base: Option<String>,
        /// Create as draft PR.
        #[serde(default)]
        draft: bool,
    },
    /// List pull requests.
    PrList {
        /// Repository directory relative to workspace root.
        repo_dir: String,
        /// Filter by state: `"open"` (default), `"closed"`, `"merged"`, `"all"`.
        state: Option<String>,
    },
    /// Fetch top-level review verdicts and PR conversation comments.
    ///
    /// Returns review approvals/rejections and top-level PR comments only.
    /// Does NOT return inline code comments on specific lines — use
    /// `pr_diff_comments` for those.
    PrReviews {
        /// Repository directory relative to workspace root.
        repo_dir: String,
        /// PR number.
        pr_number: u64,
    },
    /// Add a comment to a pull request.
    PrComment {
        /// Repository directory relative to workspace root.
        repo_dir: String,
        /// PR number.
        pr_number: u64,
        /// Comment body (Markdown).
        body: String,
    },
    /// Fetch inline code review comments on specific lines in the diff.
    ///
    /// This is the action to use when looking for code review feedback.
    /// Returns comments attached to specific file/line locations — the
    /// most actionable review feedback. `pr_reviews` does NOT include these.
    PrDiffComments {
        /// Repository directory relative to workspace root.
        repo_dir: String,
        /// PR number.
        pr_number: u64,
    },
    /// Commit staged changes with an automatic Co-authored-by trailer.
    ///
    /// Use `git add` via exec first, then this action to commit. Trailers
    /// from the configured `co_authors` list are appended automatically.
    Commit {
        /// Repository directory relative to workspace root.
        repo_dir: String,
        /// Commit message (Co-authored-by trailers are appended automatically).
        message: String,
    },
    /// Reply to an inline review comment.
    ///
    /// Use `pr_diff_comments` first to get comment IDs, then reply
    /// to a specific one. This creates a threaded reply on the same
    /// line/file, not a top-level PR comment.
    PrDiffReply {
        /// Repository directory relative to workspace root.
        repo_dir: String,
        /// PR number.
        pr_number: u64,
        /// ID of the review comment to reply to (from `pr_diff_comments`).
        comment_id: u64,
        /// Reply body (Markdown).
        body: String,
    },
}

/// A workflow run from `gh run list --json`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowRun {
    database_id: u64,
    display_title: String,
    created_at: String,
    url: String,
    workflow_name: String,
}

/// Authenticated GitHub operations.
pub struct GitHub {
    workspace_root: PathBuf,
    token: Secret,
    co_authors: Vec<String>,
}

impl GitHub {
    pub fn new(workspace_root: impl Into<PathBuf>, token: Secret, co_authors: Vec<String>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            token,
            co_authors,
        }
    }
}

impl Tool for GitHub {
    fn name(&self) -> &'static str {
        "github"
    }

    fn description(&self) -> &'static str {
        "Authenticated GitHub operations (clone, push, commit, pull requests, reviews, CI status)"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(Args)).expect("schema serialization failed")
    }

    fn execute(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let args: Args = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

            match args {
                Args::CiStatus { repo_dir, branch } => {
                    self.ci_status(&repo_dir, branch.as_deref()).await
                }
                Args::Clone { url, name } => self.clone_repo(&url, name.as_deref()).await,
                Args::Push {
                    repo_dir,
                    remote,
                    branch,
                    set_upstream,
                } => {
                    self.push(
                        &repo_dir,
                        remote.as_deref(),
                        branch.as_deref(),
                        set_upstream,
                    )
                    .await
                }
                Args::Commit { repo_dir, message } => self.commit(&repo_dir, &message).await,
                Args::PrCreate {
                    repo_dir,
                    title,
                    body,
                    base,
                    draft,
                } => {
                    self.pr_create(&repo_dir, &title, &body, base.as_deref(), draft)
                        .await
                }
                Args::PrList { repo_dir, state } => self.pr_list(&repo_dir, state.as_deref()).await,
                Args::PrReviews {
                    repo_dir,
                    pr_number,
                } => self.pr_reviews(&repo_dir, pr_number).await,
                Args::PrComment {
                    repo_dir,
                    pr_number,
                    body,
                } => self.pr_comment(&repo_dir, pr_number, &body).await,
                Args::PrDiffComments {
                    repo_dir,
                    pr_number,
                } => self.pr_diff_comments(&repo_dir, pr_number).await,
                Args::PrDiffReply {
                    repo_dir,
                    pr_number,
                    comment_id,
                    body,
                } => {
                    self.pr_diff_reply(&repo_dir, pr_number, comment_id, &body)
                        .await
                }
            }
        })
    }
}

impl GitHub {
    /// Run an authenticated git command with `GIT_ASKPASS` token injection.
    ///
    /// The token is written to a temp script in a 0700 directory and
    /// removed when `AskPass` drops (even on early return or panic).
    /// The token is on disk for the duration of one git command only.
    /// Run a git command with optional `GIT_ASKPASS` token injection.
    ///
    /// When `authenticated`, a temporary askpass script is created and
    /// removed after the command completes. Local operations (commit,
    /// branch) pass `false`.
    async fn run_git(
        &self,
        args: &[&str],
        cwd: &Path,
        label: &str,
        authenticated: bool,
    ) -> Result<String, ToolError> {
        let mut cmd = Command::new("git");
        cmd.args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(super::safe_env());

        let askpass = if authenticated {
            let ap = AskPass::create(&self.token).await?;
            cmd.env("GIT_ASKPASS", ap.path())
                .env("GIT_TERMINAL_PROMPT", "0");
            Some(ap)
        } else {
            None
        };

        let output = exec_cmd(&mut cmd, label).await?;
        drop(askpass);
        format_cmd(label, &output)
    }

    /// Run an authenticated `gh` CLI command with `GH_TOKEN` env injection.
    ///
    /// `args` are passed directly to `gh`. `cwd` sets the working
    /// directory. Returns stdout on success.
    ///
    /// The token lives in the child process environment for the duration
    /// of the `gh` command (visible via `/proc/<pid>/environ` to the same
    /// user). There is no `GH_ASKPASS` equivalent — `GH_TOKEN` env is
    /// the `gh` CLI's intended auth mechanism. The alternative
    /// (`gh auth login`) persists the token to `~/.config/gh/hosts.yml`,
    /// which is strictly worse.
    async fn run_gh(&self, args: &[&str], cwd: &Path, label: &str) -> Result<String, ToolError> {
        let mut cmd = Command::new("gh");
        cmd.args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(super::safe_env())
            .env("GH_TOKEN", self.token.expose())
            .env("GH_PROMPT_DISABLED", "1")
            .env("NO_COLOR", "1");

        let output = exec_cmd(&mut cmd, label).await?;
        format_cmd(label, &output)
    }

    /// Resolve and validate a repo directory within the workspace.
    fn resolve_repo_dir(&self, repo_dir: &str) -> Result<PathBuf, ToolError> {
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

    /// Fetch the latest failed CI run and its failure logs.
    async fn ci_status(&self, repo_dir: &str, branch: Option<&str>) -> Result<String, ToolError> {
        let cwd = self.resolve_repo_dir(repo_dir)?;

        let branch_name = match branch {
            Some(b) => b.to_string(),
            None => current_branch(&cwd).await?,
        };

        // Find the latest failed run on the branch.
        let list_label = format!("gh run list --branch {branch_name} --status failure");
        let mut cmd = Command::new("gh");
        cmd.args([
            "run",
            "list",
            "--branch",
            &branch_name,
            "--status",
            "failure",
            "--limit",
            "1",
            "--json",
            "databaseId,displayTitle,createdAt,url,workflowName",
        ])
        .current_dir(&cwd)
        .env_clear()
        .envs(super::safe_env())
        .env("GH_TOKEN", self.token.expose())
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1");

        let list = exec_cmd(&mut cmd, &list_label).await?;
        if list.exit_code != 0 {
            return Err(ToolError::ExecutionFailed(format!(
                "{list_label}: {}",
                list.stderr
            )));
        }

        let runs: Vec<WorkflowRun> = serde_json::from_str(&list.stdout)
            .map_err(|e| ToolError::ExecutionFailed(format!("failed to parse run list: {e}")))?;

        let run = runs.first().ok_or_else(|| {
            ToolError::ExecutionFailed(format!("no failed runs on branch `{branch_name}`"))
        })?;

        let id_str = run.database_id.to_string();
        let logs = self
            .run_gh(
                &["run", "view", &id_str, "--log-failed"],
                &cwd,
                &format!("gh run view {} --log-failed", run.database_id),
            )
            .await?;

        let mut output = format!(
            "Run #{}: \"{}\" ({})\n\
             Created: {}\n\
             URL: {}\n\n\
             ---\n\n",
            run.database_id, run.display_title, run.workflow_name, run.created_at, run.url
        );
        output.push_str(&logs);

        Ok(output)
    }

    /// Clone a repository into `projects/<name>`.
    async fn clone_repo(&self, url: &str, name: Option<&str>) -> Result<String, ToolError> {
        let https_url = to_https_url(url)?;
        let repo_name = match name {
            Some(n) => validate_name(n)?.to_string(),
            None => extract_repo_name(&https_url)?,
        };

        let projects_dir = self.workspace_root.join("projects");
        let target = projects_dir.join(&repo_name);

        if target.exists() {
            return Err(ToolError::ExecutionFailed(format!(
                "projects/{repo_name} already exists"
            )));
        }

        // Ensure projects/ exists.
        tokio::fs::create_dir_all(&projects_dir)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("mkdir projects/: {e}")))?;

        self.run_git(
            &["clone", "--", &https_url, &repo_name],
            &projects_dir,
            &format!("git clone {https_url} projects/{repo_name}"),
            true,
        )
        .await
    }

    /// Push commits to a remote.
    async fn push(
        &self,
        repo_dir: &str,
        remote: Option<&str>,
        branch: Option<&str>,
        set_upstream: bool,
    ) -> Result<String, ToolError> {
        let cwd = self.resolve_repo_dir(repo_dir)?;

        let remote = remote.unwrap_or("origin");
        let mut args = vec!["push"];

        if set_upstream {
            args.push("--set-upstream");
        }

        args.push(remote);

        if let Some(b) = branch {
            args.push(b);
        }

        let label = format!("git {}", args.join(" "));
        self.run_git(&args, &cwd, &label, true).await
    }

    /// Commit staged changes with Co-authored-by trailers.
    async fn commit(&self, repo_dir: &str, message: &str) -> Result<String, ToolError> {
        let cwd = self.resolve_repo_dir(repo_dir)?;
        let full_message = format_commit_message(message, &self.co_authors);
        self.run_git(&["commit", "-m", &full_message], &cwd, "git commit", false)
            .await
    }

    /// Create a pull request via `gh pr create`.
    async fn pr_create(
        &self,
        repo_dir: &str,
        title: &str,
        body: &str,
        base: Option<&str>,
        draft: bool,
    ) -> Result<String, ToolError> {
        let cwd = self.resolve_repo_dir(repo_dir)?;

        let mut args = vec!["pr", "create", "--title", title, "--body", body];

        if let Some(b) = base {
            args.extend(["--base", b]);
        }
        if draft {
            args.push("--draft");
        }

        self.run_gh(&args, &cwd, "gh pr create").await
    }

    /// List pull requests via `gh pr list`.
    async fn pr_list(&self, repo_dir: &str, state: Option<&str>) -> Result<String, ToolError> {
        let cwd = self.resolve_repo_dir(repo_dir)?;

        let state = state.unwrap_or("open");
        let valid_states = ["open", "closed", "merged", "all"];
        if !valid_states.contains(&state) {
            return Err(ToolError::InvalidArguments(format!(
                "invalid state: {state} (expected one of: {})",
                valid_states.join(", ")
            )));
        }

        self.run_gh(
            &[
                "pr",
                "list",
                "--state",
                state,
                "--json",
                "number,title,state,url",
            ],
            &cwd,
            &format!("gh pr list --state {state}"),
        )
        .await
    }

    /// Fetch reviews and comments for a pull request via `gh pr view`.
    async fn pr_reviews(&self, repo_dir: &str, pr_number: u64) -> Result<String, ToolError> {
        let cwd = self.resolve_repo_dir(repo_dir)?;
        let number = pr_number.to_string();

        self.run_gh(
            &[
                "pr",
                "view",
                &number,
                "--json",
                "reviews,reviewRequests,comments",
            ],
            &cwd,
            &format!("gh pr view {number} --json reviews,reviewRequests,comments"),
        )
        .await
    }

    /// Add a comment to a pull request via `gh pr comment`.
    async fn pr_comment(
        &self,
        repo_dir: &str,
        pr_number: u64,
        body: &str,
    ) -> Result<String, ToolError> {
        let cwd = self.resolve_repo_dir(repo_dir)?;
        let number = pr_number.to_string();

        self.run_gh(
            &["pr", "comment", &number, "--body", body],
            &cwd,
            &format!("gh pr comment {number}"),
        )
        .await
    }

    /// Fetch inline review comments (line-level) via the REST API.
    ///
    /// `gh pr view --json` has no field for these — they live at a
    /// separate REST endpoint. `gh api` resolves `{owner}` and `{repo}`
    /// from the git remote when run inside a repository directory.
    async fn pr_diff_comments(&self, repo_dir: &str, pr_number: u64) -> Result<String, ToolError> {
        let cwd = self.resolve_repo_dir(repo_dir)?;
        let endpoint = format!("repos/{{owner}}/{{repo}}/pulls/{pr_number}/comments");

        self.run_gh(
            &["api", &endpoint],
            &cwd,
            &format!("gh api ...pulls/{pr_number}/comments"),
        )
        .await
    }

    /// Reply to an inline review comment via the REST API.
    ///
    /// Creates a threaded reply on the same file/line as the original
    /// comment. The `comment_id` comes from the `id` field in the
    /// `pr_diff_comments` response.
    async fn pr_diff_reply(
        &self,
        repo_dir: &str,
        pr_number: u64,
        comment_id: u64,
        body: &str,
    ) -> Result<String, ToolError> {
        let cwd = self.resolve_repo_dir(repo_dir)?;
        let endpoint =
            format!("repos/{{owner}}/{{repo}}/pulls/{pr_number}/comments/{comment_id}/replies");
        let body_field = format!("body={body}");

        self.run_gh(
            &["api", &endpoint, "-f", &body_field],
            &cwd,
            &format!("gh api ...comments/{comment_id}/replies"),
        )
        .await
    }
}

// ── Command execution ───────────────────────────────────────────────

/// Raw output from a subprocess.
struct CmdOutput {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

/// Run a command with timeout and collect output.
///
/// Returns `CmdOutput` on both success and failure — the caller
/// decides how to present it (envelope for the LLM, raw parsing,
/// etc.). Returns `ToolError` only for launch failures and timeouts.
async fn exec_cmd(cmd: &mut Command, label: &str) -> Result<CmdOutput, ToolError> {
    debug!(label, "Running command");

    let output = timeout(Duration::from_secs(TIMEOUT_SECS), cmd.output())
        .await
        .map_err(|_| ToolError::Timeout)?
        .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

    Ok(CmdOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code().unwrap_or(-1),
    })
}

/// Format command output as `$ label\nstdout\nstderr\nExit code: N`.
///
/// On non-zero exit, returns `ToolError::ExecutionFailed` with the
/// formatted output so the LLM sees what went wrong.
fn format_cmd(label: &str, output: &CmdOutput) -> Result<String, ToolError> {
    let mut result = format!("$ {label}\n");

    if !output.stdout.is_empty() {
        result.push_str(&super::truncate_output(&output.stdout, MAX_OUTPUT_BYTES));
    }
    if !output.stderr.is_empty() {
        if !output.stdout.is_empty() {
            result.push('\n');
        }
        result.push_str(&super::truncate_output(&output.stderr, MAX_OUTPUT_BYTES));
    }

    let _ = write!(result, "\nExit code: {}", output.exit_code);

    if output.exit_code != 0 {
        return Err(ToolError::ExecutionFailed(result));
    }

    Ok(result)
}

// ── Commit message formatting ───────────────────────────────────────

/// Append `Co-authored-by` trailers to a commit message.
///
/// Returns the message unchanged when `co_authors` is empty. Otherwise
/// appends a blank line followed by one trailer per co-author.
fn format_commit_message(message: &str, co_authors: &[String]) -> String {
    if co_authors.is_empty() {
        return message.to_string();
    }

    let trailer_len: usize = co_authors.iter().map(|a| a.len() + 18).sum();
    let mut msg = String::with_capacity(message.len() + 2 + trailer_len);
    msg.push_str(message);
    msg.push_str("\n\n");
    for author in co_authors {
        msg.push_str("Co-authored-by: ");
        msg.push_str(author);
        msg.push('\n');
    }
    msg
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

// ── Branch resolution ────────────────────────────────────────────────

/// Get the current branch name from a git working directory.
async fn current_branch(cwd: &Path) -> Result<String, ToolError> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .await
        .map_err(|e| ToolError::ExecutionFailed(format!("git rev-parse: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ToolError::ExecutionFailed(format!(
            "failed to get current branch: {stderr}"
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

// ── URL handling ────────────────────────────────────────────────────

/// Convert SSH-style URLs to HTTPS. Passes HTTPS URLs through unchanged.
///
/// Handles:
/// - `git@github.com:owner/repo.git` → `https://github.com/owner/repo.git`
/// - `ssh://git@github.com/owner/repo.git` → `https://github.com/owner/repo.git`
/// - `https://github.com/owner/repo.git` → unchanged
fn to_https_url(url: &str) -> Result<String, ToolError> {
    // Already HTTPS
    if url.starts_with("https://") {
        return Ok(url.to_string());
    }

    // SCP-style: git@github.com:owner/repo.git
    if let Some(rest) = url.strip_prefix("git@")
        && let Some((host, path)) = rest.split_once(':')
    {
        return Ok(format!("https://{host}/{path}"));
    }

    // ssh://git@github.com/owner/repo.git
    if let Some(rest) = url.strip_prefix("ssh://git@") {
        return Ok(format!("https://{rest}"));
    }

    Err(ToolError::InvalidArguments(format!(
        "unsupported URL scheme: {url}"
    )))
}

/// Extract the repository name from an HTTPS URL.
///
/// `https://github.com/owner/repo.git` → `repo`
/// `https://github.com/owner/repo` → `repo`
fn extract_repo_name(url: &str) -> Result<String, ToolError> {
    let path = url
        .strip_prefix("https://")
        .unwrap_or(url)
        .trim_end_matches('/')
        .trim_end_matches(".git");

    path.rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(String::from)
        .ok_or_else(|| ToolError::InvalidArguments(format!("cannot extract repo name from: {url}")))
}

/// Validate a user-provided directory name.
///
/// Rejects path traversal, absolute paths, and slashes.
fn validate_name(name: &str) -> Result<&str, ToolError> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || name.starts_with('.')
        || name.starts_with('-')
    {
        return Err(ToolError::InvalidArguments(format!(
            "invalid directory name: {name}"
        )));
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── URL conversion ──────────────────────────────────────────────

    #[test]
    fn https_url_passthrough() {
        let url = "https://github.com/owner/repo.git";
        assert_eq!(to_https_url(url).unwrap(), url);
    }

    #[test]
    fn scp_style_to_https() {
        assert_eq!(
            to_https_url("git@github.com:owner/repo.git").unwrap(),
            "https://github.com/owner/repo.git"
        );
    }

    #[test]
    fn ssh_url_to_https() {
        assert_eq!(
            to_https_url("ssh://git@github.com/owner/repo.git").unwrap(),
            "https://github.com/owner/repo.git"
        );
    }

    #[test]
    fn unsupported_scheme_rejected() {
        assert!(to_https_url("ftp://example.com/repo").is_err());
    }

    // ── Repo name extraction ────────────────────────────────────────

    #[test]
    fn extract_name_with_git_suffix() {
        assert_eq!(
            extract_repo_name("https://github.com/owner/repo.git").unwrap(),
            "repo"
        );
    }

    #[test]
    fn extract_name_without_git_suffix() {
        assert_eq!(
            extract_repo_name("https://github.com/owner/repo").unwrap(),
            "repo"
        );
    }

    #[test]
    fn extract_name_trailing_slash() {
        assert_eq!(
            extract_repo_name("https://github.com/owner/repo/").unwrap(),
            "repo"
        );
    }

    // ── Name validation ─────────────────────────────────────────────

    #[test]
    fn valid_name() {
        assert_eq!(validate_name("myrepo").unwrap(), "myrepo");
        assert_eq!(validate_name("my_repo").unwrap(), "my_repo");
        assert_eq!(validate_name("my-repo").unwrap(), "my-repo");
    }

    #[test]
    fn reject_traversal() {
        assert!(validate_name("..").is_err());
        assert!(validate_name("../escape").is_err());
    }

    #[test]
    fn reject_slashes() {
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("a\\b").is_err());
    }

    #[test]
    fn reject_hidden() {
        assert!(validate_name(".hidden").is_err());
    }

    #[test]
    fn reject_dash_prefix() {
        assert!(validate_name("-flag").is_err());
    }

    #[test]
    fn reject_empty() {
        assert!(validate_name("").is_err());
    }

    // ── Schema ──────────────────────────────────────────────────────

    #[test]
    fn schema_requires_action() {
        let schema = serde_json::to_value(schemars::schema_for!(Args)).unwrap();
        // Tagged enum — should have oneOf or use discriminator
        assert!(
            schema.to_string().contains("action"),
            "schema must include action discriminator: {schema}"
        );
    }

    // ── CiStatus deserialization ──────────────────────────────────

    #[test]
    fn deserialize_ci_status_minimal() {
        let json = serde_json::json!({
            "action": "ci_status",
            "repo_dir": "projects/myrepo"
        });
        let args: Args = serde_json::from_value(json).unwrap();
        assert!(matches!(
            args,
            Args::CiStatus {
                repo_dir,
                branch: None,
            } if repo_dir == "projects/myrepo"
        ));
    }

    #[test]
    fn deserialize_ci_status_with_branch() {
        let json = serde_json::json!({
            "action": "ci_status",
            "repo_dir": "projects/myrepo",
            "branch": "feature-xyz"
        });
        let args: Args = serde_json::from_value(json).unwrap();
        assert!(matches!(
            args,
            Args::CiStatus {
                branch: Some(b),
                ..
            } if b == "feature-xyz"
        ));
    }

    // ── Clone deserialization ──────────────────────────────────────

    #[test]
    fn deserialize_clone_args() {
        let json = serde_json::json!({
            "action": "clone",
            "url": "https://github.com/owner/repo.git"
        });
        let args: Args = serde_json::from_value(json).unwrap();
        assert!(
            matches!(args, Args::Clone { url, name } if url.contains("owner/repo") && name.is_none())
        );
    }

    #[test]
    fn deserialize_clone_with_name() {
        let json = serde_json::json!({
            "action": "clone",
            "url": "https://github.com/owner/repo.git",
            "name": "custom"
        });
        let args: Args = serde_json::from_value(json).unwrap();
        assert!(matches!(args, Args::Clone { name: Some(n), .. } if n == "custom"));
    }

    // ── Push deserialization ────────────────────────────────────────

    #[test]
    fn deserialize_push_minimal() {
        let json = serde_json::json!({
            "action": "push",
            "repo_dir": "projects/myrepo"
        });
        let args: Args = serde_json::from_value(json).unwrap();
        assert!(matches!(
            args,
            Args::Push {
                repo_dir,
                remote: None,
                branch: None,
                set_upstream: false,
            } if repo_dir == "projects/myrepo"
        ));
    }

    #[test]
    fn deserialize_push_full() {
        let json = serde_json::json!({
            "action": "push",
            "repo_dir": "projects/myrepo",
            "remote": "upstream",
            "branch": "feature",
            "set_upstream": true
        });
        let args: Args = serde_json::from_value(json).unwrap();
        assert!(matches!(
            args,
            Args::Push {
                remote: Some(r),
                branch: Some(b),
                set_upstream: true,
                ..
            } if r == "upstream" && b == "feature"
        ));
    }

    // ── Repo dir validation ─────────────────────────────────────────

    #[test]
    fn resolve_repo_dir_rejects_traversal() {
        let gh = make_github(tempfile::tempdir().unwrap().path());
        assert!(matches!(
            gh.resolve_repo_dir("../escape"),
            Err(ToolError::Blocked(_))
        ));
    }

    #[test]
    fn resolve_repo_dir_rejects_absolute() {
        let gh = make_github(tempfile::tempdir().unwrap().path());
        assert!(matches!(
            gh.resolve_repo_dir("/etc"),
            Err(ToolError::Blocked(_))
        ));
    }

    #[test]
    fn resolve_repo_dir_rejects_non_repo() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("projects/notrepo")).unwrap();
        let gh = make_github(dir.path());
        assert!(matches!(
            gh.resolve_repo_dir("projects/notrepo"),
            Err(ToolError::InvalidArguments(_))
        ));
    }

    #[test]
    fn resolve_repo_dir_accepts_valid_repo() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("projects/myrepo/.git")).unwrap();
        let gh = make_github(dir.path());
        let resolved = gh.resolve_repo_dir("projects/myrepo").unwrap();
        assert!(resolved.ends_with("projects/myrepo"));
    }

    // ── PrCreate deserialization ──────────────────────────────────

    #[test]
    fn deserialize_pr_create_minimal() {
        let json = serde_json::json!({
            "action": "pr_create",
            "repo_dir": "projects/myrepo",
            "title": "Fix bug",
            "body": "Fixes the thing"
        });
        let args: Args = serde_json::from_value(json).unwrap();
        assert!(matches!(
            args,
            Args::PrCreate {
                title,
                body,
                base: None,
                draft: false,
                ..
            } if title == "Fix bug" && body == "Fixes the thing"
        ));
    }

    #[test]
    fn deserialize_pr_create_full() {
        let json = serde_json::json!({
            "action": "pr_create",
            "repo_dir": "projects/myrepo",
            "title": "Feature",
            "body": "Add feature",
            "base": "develop",
            "draft": true
        });
        let args: Args = serde_json::from_value(json).unwrap();
        assert!(matches!(
            args,
            Args::PrCreate {
                base: Some(b),
                draft: true,
                ..
            } if b == "develop"
        ));
    }

    // ── PrList deserialization ───────────────────────────────────

    #[test]
    fn deserialize_pr_list_minimal() {
        let json = serde_json::json!({
            "action": "pr_list",
            "repo_dir": "projects/myrepo"
        });
        let args: Args = serde_json::from_value(json).unwrap();
        assert!(matches!(
            args,
            Args::PrList { repo_dir, state: None } if repo_dir == "projects/myrepo"
        ));
    }

    #[test]
    fn deserialize_pr_list_with_state() {
        let json = serde_json::json!({
            "action": "pr_list",
            "repo_dir": "projects/myrepo",
            "state": "closed"
        });
        let args: Args = serde_json::from_value(json).unwrap();
        assert!(matches!(
            args,
            Args::PrList { state: Some(s), .. } if s == "closed"
        ));
    }

    #[test]
    fn pr_list_rejects_invalid_state() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("projects/r/.git")).unwrap();
        let gh = make_github(dir.path());

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(gh.pr_list("projects/r", Some("bogus")));
        assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
    }

    // ── PrReviews deserialization ───────────────────────────────

    #[test]
    fn deserialize_pr_reviews() {
        let json = serde_json::json!({
            "action": "pr_reviews",
            "repo_dir": "projects/myrepo",
            "pr_number": 42
        });
        let args: Args = serde_json::from_value(json).unwrap();
        assert!(matches!(args, Args::PrReviews { pr_number: 42, .. }));
    }

    // ── PrComment deserialization ───────────────────────────────

    #[test]
    fn deserialize_pr_comment() {
        let json = serde_json::json!({
            "action": "pr_comment",
            "repo_dir": "projects/myrepo",
            "pr_number": 7,
            "body": "LGTM"
        });
        let args: Args = serde_json::from_value(json).unwrap();
        assert!(matches!(
            args,
            Args::PrComment { pr_number: 7, body, .. } if body == "LGTM"
        ));
    }

    // ── PrDiffComments deserialization ────────────────────────────

    #[test]
    fn deserialize_pr_diff_comments() {
        let json = serde_json::json!({
            "action": "pr_diff_comments",
            "repo_dir": "projects/myrepo",
            "pr_number": 99
        });
        let args: Args = serde_json::from_value(json).unwrap();
        assert!(matches!(args, Args::PrDiffComments { pr_number: 99, .. }));
    }

    // ── PrDiffReply deserialization ────────────────────────────

    #[test]
    fn deserialize_pr_diff_reply() {
        let json = serde_json::json!({
            "action": "pr_diff_reply",
            "repo_dir": "projects/myrepo",
            "pr_number": 5,
            "comment_id": 123_456,
            "body": "Fixed in the latest push"
        });
        let args: Args = serde_json::from_value(json).unwrap();
        assert!(matches!(
            args,
            Args::PrDiffReply {
                pr_number: 5,
                comment_id: 123_456,
                body,
                ..
            } if body == "Fixed in the latest push"
        ));
    }

    // ── Commit deserialization ──────────────────────────────────

    #[test]
    fn deserialize_commit_minimal() {
        let json = serde_json::json!({
            "action": "commit",
            "repo_dir": "projects/myrepo",
            "message": "Fix the thing"
        });
        let args: Args = serde_json::from_value(json).unwrap();
        assert!(matches!(
            args,
            Args::Commit { repo_dir, message }
                if repo_dir == "projects/myrepo" && message == "Fix the thing"
        ));
    }

    // ── Co-author trailer formatting ────────────────────────────

    #[test]
    fn format_message_no_co_authors() {
        let msg = format_commit_message("Fix bug", &[]);
        assert_eq!(msg, "Fix bug");
    }

    #[test]
    fn format_message_one_co_author() {
        let authors = ["Alice <alice@example.com>".to_string()];
        let msg = format_commit_message("Fix bug", &authors);
        assert_eq!(
            msg,
            "Fix bug\n\nCo-authored-by: Alice <alice@example.com>\n"
        );
    }

    #[test]
    fn format_message_multiple_co_authors() {
        let authors = [
            "Alice <alice@example.com>".to_string(),
            "Bob <bob@example.com>".to_string(),
        ];
        let msg = format_commit_message("Add feature", &authors);
        assert_eq!(
            msg,
            "Add feature\n\nCo-authored-by: Alice <alice@example.com>\nCo-authored-by: Bob <bob@example.com>\n"
        );
    }

    /// Helper to build a GitHub instance for tests.
    fn make_github(workspace: &Path) -> GitHub {
        use crate::secrets::Secret;
        GitHub::new(workspace, Secret::test("fake-token"), vec![])
    }
}
