//! GitHub integration tools.
//!
//! Provides authenticated git and GitHub CLI operations. The token never
//! reaches the exec tool — it is injected only into subprocesses spawned
//! by this module via `GIT_ASKPASS` (for git) or `GH_TOKEN` (for `gh`).
//!
//! # Architecture
//!
//! [`api::GitHubApi`] is the raw subprocess boundary — it owns credentials
//! and spawns `gh`/`git` processes. [`GitHub<A>`] is the business logic
//! layer that assembles arguments, parses JSON, and formats output.
//! Tests substitute `StubGitHubApi` to exercise the logic without
//! spawning real subprocesses.
//!
//! # Token injection
//!
//! For `git clone`/`push`, a temporary helper script is written to a
//! private directory, set as `GIT_ASKPASS`, and deleted immediately after
//! the subprocess exits. The script prints the token to stdout when
//! invoked by git. The token is on disk for the duration of one git
//! command only.

mod api;
mod client;
#[cfg(test)]
mod test_helpers;
mod types;
mod url;

use url::{extract_repo_name, format_commit_message, to_https_url, validate_name};

pub use api::{GitHubApi, RealGitHubApi};
pub use client::GitHubClient;
use types::{DiffComment, PrReviewsResponse, PullRequest, WorkflowRun};

use std::fmt::Write;
use std::path::PathBuf;

use schemars::JsonSchema;
use serde::Deserialize;

use std::future::Future;
use std::pin::Pin;

use super::Tool;
use crate::error::ToolError;

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

// ── Business logic layer ────────────────────────────────────────────

/// Authenticated GitHub operations (temporary monolithic tool).
///
/// Delegates to [`GitHubClient`] for subprocess plumbing and wraps it
/// with the [`Tool`] trait. Will be replaced by per-action tools.
pub struct GitHub<A> {
    client: GitHubClient<A>,
}

impl<A: GitHubApi> GitHub<A> {
    pub fn new(api: A, workspace_root: impl Into<PathBuf>, co_authors: Vec<String>) -> Self {
        Self {
            client: GitHubClient::new(api, workspace_root, co_authors),
        }
    }
}

impl<A: GitHubApi> Tool for GitHub<A> {
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

impl<A: GitHubApi> GitHub<A> {
    /// Fetch the latest failed CI run and its failure logs.
    async fn ci_status(&self, repo_dir: &str, branch: Option<&str>) -> Result<String, ToolError> {
        let cwd = self.client.resolve_repo_dir(repo_dir)?;

        let branch_name = match branch {
            Some(b) => b.to_string(),
            None => self.client.current_branch(&cwd).await?,
        };

        // Find the latest failed run on the branch.
        let runs: Vec<WorkflowRun> = self
            .client
            .run_gh_json(
                &[
                    "run",
                    "list",
                    "--branch",
                    &branch_name,
                    "--status",
                    "failure",
                    "--limit",
                    "1",
                ],
                "databaseId,displayTitle,createdAt,url,workflowName",
                &cwd,
            )
            .await?;

        let run = runs.first().ok_or_else(|| {
            ToolError::ExecutionFailed(format!("no failed runs on branch `{branch_name}`"))
        })?;

        let id_str = run.database_id.to_string();
        let logs = self
            .client
            .run_gh(&["run", "view", &id_str, "--log-failed"], &cwd)
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

        let projects_dir = self.client.workspace_root.join("projects");
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

        self.client
            .run_git(
                &["clone", "--", &https_url, &repo_name],
                &projects_dir,
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
        let cwd = self.client.resolve_repo_dir(repo_dir)?;

        let remote = remote.unwrap_or("origin");
        let mut args = vec!["push"];

        if set_upstream {
            args.push("--set-upstream");
        }

        args.push(remote);

        if let Some(b) = branch {
            args.push(b);
        }

        self.client.run_git(&args, &cwd, true).await
    }

    /// Commit staged changes with Co-authored-by trailers.
    async fn commit(&self, repo_dir: &str, message: &str) -> Result<String, ToolError> {
        let cwd = self.client.resolve_repo_dir(repo_dir)?;
        let full_message = format_commit_message(message, &self.client.co_authors);
        self.client
            .run_git(&["commit", "-m", &full_message], &cwd, false)
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
        let cwd = self.client.resolve_repo_dir(repo_dir)?;

        let mut args = vec!["pr", "create", "--title", title, "--body", body];

        if let Some(b) = base {
            args.extend(["--base", b]);
        }
        if draft {
            args.push("--draft");
        }

        self.client.run_gh(&args, &cwd).await
    }

    /// List pull requests via `gh pr list`.
    async fn pr_list(&self, repo_dir: &str, state: Option<&str>) -> Result<String, ToolError> {
        let cwd = self.client.resolve_repo_dir(repo_dir)?;

        let state = state.unwrap_or("open");
        let valid_states = ["open", "closed", "merged", "all"];
        if !valid_states.contains(&state) {
            return Err(ToolError::InvalidArguments(format!(
                "invalid state: {state} (expected one of: {})",
                valid_states.join(", ")
            )));
        }

        let prs: Vec<PullRequest> = self
            .client
            .run_gh_json(
                &["pr", "list", "--state", state],
                "number,title,state,url",
                &cwd,
            )
            .await?;

        if prs.is_empty() {
            return Ok(format!("No {state} pull requests."));
        }

        Ok(prs
            .iter()
            .map(|pr| format!("#{} {} [{}]\n  {}", pr.number, pr.title, pr.state, pr.url))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    /// Fetch reviews and comments for a pull request via `gh pr view`.
    async fn pr_reviews(&self, repo_dir: &str, pr_number: u64) -> Result<String, ToolError> {
        let cwd = self.client.resolve_repo_dir(repo_dir)?;
        let number = pr_number.to_string();

        let resp: PrReviewsResponse = self
            .client
            .run_gh_json(
                &["pr", "view", &number],
                "reviews,reviewRequests,comments",
                &cwd,
            )
            .await?;

        let mut output = String::new();

        if !resp.review_requests.is_empty() {
            output.push_str("Pending reviewers: ");
            let names: Vec<&str> = resp
                .review_requests
                .iter()
                .map(|r| {
                    r.login
                        .as_deref()
                        .or(r.name.as_deref())
                        .unwrap_or("unknown")
                })
                .collect();
            output.push_str(&names.join(", "));
            output.push_str("\n\n");
        }

        for r in &resp.reviews {
            let _ = writeln!(
                output,
                "@{} {} ({})",
                r.author.login, r.state, r.submitted_at
            );
            if !r.body.is_empty() {
                let _ = writeln!(output, "{}", r.body);
            }
            output.push('\n');
        }

        for c in &resp.comments {
            let _ = writeln!(output, "@{} ({})\n{}", c.author.login, c.created_at, c.body);
            output.push('\n');
        }

        if output.is_empty() {
            return Ok(format!("No reviews or comments on PR #{pr_number}."));
        }

        Ok(output)
    }

    /// Add a comment to a pull request via `gh pr comment`.
    async fn pr_comment(
        &self,
        repo_dir: &str,
        pr_number: u64,
        body: &str,
    ) -> Result<String, ToolError> {
        let cwd = self.client.resolve_repo_dir(repo_dir)?;
        let number = pr_number.to_string();

        self.client
            .run_gh(&["pr", "comment", &number, "--body", body], &cwd)
            .await
    }

    /// Fetch inline review comments (line-level) via the REST API.
    ///
    /// `gh pr view --json` has no field for these — they live at a
    /// separate REST endpoint. `gh api` resolves `{owner}` and `{repo}`
    /// from the git remote when run inside a repository directory.
    async fn pr_diff_comments(&self, repo_dir: &str, pr_number: u64) -> Result<String, ToolError> {
        let cwd = self.client.resolve_repo_dir(repo_dir)?;
        let endpoint = format!("repos/{{owner}}/{{repo}}/pulls/{pr_number}/comments");

        let comments: Vec<DiffComment> = self.client.run_gh_api(&endpoint, &cwd).await?;

        if comments.is_empty() {
            return Ok(format!("No inline comments on PR #{pr_number}."));
        }

        Ok(comments
            .iter()
            .map(|c| {
                let location = c.line.map_or(c.path.clone(), |l| format!("{}:{l}", c.path));
                format!(
                    "[id:{}] @{} at {}\n{}",
                    c.id, c.user.login, location, c.body
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n"))
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
        let cwd = self.client.resolve_repo_dir(repo_dir)?;
        let endpoint =
            format!("repos/{{owner}}/{{repo}}/pulls/{pr_number}/comments/{comment_id}/replies");
        let body_field = format!("body={body}");

        self.client
            .run_gh(&["api", &endpoint, "-f", &body_field], &cwd)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::test_helpers::{ok_output, stub_with_repo};
    use super::*;

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
        let (gh, repo) = stub_with_repo(vec![]);

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(gh.pr_list(&repo, Some("bogus")));
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

    // ── pr_list ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn pr_list_formats_output() {
        let json = serde_json::to_string(&serde_json::json!([
            {"number": 1, "title": "Fix bug", "state": "OPEN", "url": "https://github.com/o/r/pull/1"},
            {"number": 2, "title": "Add feature", "state": "OPEN", "url": "https://github.com/o/r/pull/2"},
        ]))
        .unwrap();

        let (gh, repo) = stub_with_repo(vec![ok_output(&json)]);
        let result = gh.pr_list(&repo, None).await.unwrap();
        assert_eq!(
            result,
            "\
#1 Fix bug [OPEN]
  https://github.com/o/r/pull/1
#2 Add feature [OPEN]
  https://github.com/o/r/pull/2"
        );
    }

    #[tokio::test]
    async fn pr_list_empty_response() {
        let (gh, repo) = stub_with_repo(vec![ok_output("[]")]);
        let result = gh.pr_list(&repo, None).await.unwrap();
        assert_eq!(result, "No open pull requests.");
    }

    // ── pr_reviews ───────────────────────────────────────────────────

    #[tokio::test]
    async fn pr_reviews_formats_reviews_and_comments() {
        let json = serde_json::to_string(&serde_json::json!({
            "reviews": [{
                "author": {"login": "alice"},
                "body": "Looks good",
                "state": "APPROVED",
                "submittedAt": "2025-01-15T10:00:00Z"
            }],
            "reviewRequests": [{"login": "bob", "name": null}],
            "comments": [{
                "author": {"login": "carol"},
                "body": "What about edge cases?",
                "createdAt": "2025-01-15T11:00:00Z"
            }]
        }))
        .unwrap();

        let (gh, repo) = stub_with_repo(vec![ok_output(&json)]);
        let result = gh.pr_reviews(&repo, 42).await.unwrap();
        assert_eq!(
            result,
            "\
Pending reviewers: bob

@alice APPROVED (2025-01-15T10:00:00Z)
Looks good

@carol (2025-01-15T11:00:00Z)
What about edge cases?

"
        );
    }

    #[tokio::test]
    async fn pr_reviews_empty() {
        let json = serde_json::to_string(&serde_json::json!({
            "reviews": [],
            "reviewRequests": [],
            "comments": []
        }))
        .unwrap();

        let (gh, repo) = stub_with_repo(vec![ok_output(&json)]);
        let result = gh.pr_reviews(&repo, 1).await.unwrap();
        assert_eq!(result, "No reviews or comments on PR #1.");
    }

    // ── pr_diff_comments ─────────────────────────────────────────────

    #[tokio::test]
    async fn pr_diff_comments_formats_output() {
        let json = serde_json::to_string(&serde_json::json!([
            {"id": 100, "path": "src/main.rs", "line": 42, "body": "Nit: rename this", "user": {"login": "alice"}},
            {"id": 101, "path": "src/lib.rs", "line": null, "body": "Outdated", "user": {"login": "bob"}}
        ]))
        .unwrap();

        let (gh, repo) = stub_with_repo(vec![ok_output(&json)]);
        let result = gh.pr_diff_comments(&repo, 5).await.unwrap();
        assert_eq!(
            result,
            "\
[id:100] @alice at src/main.rs:42
Nit: rename this

[id:101] @bob at src/lib.rs
Outdated"
        );
    }

    #[tokio::test]
    async fn pr_diff_comments_empty() {
        let (gh, repo) = stub_with_repo(vec![ok_output("[]")]);
        let result = gh.pr_diff_comments(&repo, 5).await.unwrap();
        assert_eq!(result, "No inline comments on PR #5.");
    }

    // ── ci_status ────────────────────────────────────────────────────

    #[tokio::test]
    async fn ci_status_formats_run_and_logs() {
        let runs_json = serde_json::to_string(&serde_json::json!([{
            "databaseId": 9999,
            "displayTitle": "CI",
            "createdAt": "2025-01-15T10:00:00Z",
            "url": "https://github.com/o/r/actions/runs/9999",
            "workflowName": "test"
        }]))
        .unwrap();

        let log_output = "test-job  Step failed";

        let (gh, repo) = stub_with_repo(vec![ok_output(&runs_json), ok_output(log_output)]);
        let result = gh.ci_status(&repo, Some("main")).await.unwrap();
        assert_eq!(
            result,
            "\
Run #9999: \"CI\" (test)
Created: 2025-01-15T10:00:00Z
URL: https://github.com/o/r/actions/runs/9999

---

$ stub
test-job  Step failed
Exit code: 0"
        );
    }

    #[tokio::test]
    async fn ci_status_no_failed_runs() {
        let (gh, repo) = stub_with_repo(vec![ok_output("[]")]);
        let result = gh.ci_status(&repo, Some("main")).await;
        assert!(
            matches!(result, Err(ToolError::ExecutionFailed(msg)) if msg.contains("no failed runs"))
        );
    }
}
