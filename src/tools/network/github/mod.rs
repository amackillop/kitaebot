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
mod ci_status;
mod client;
mod commit;
mod git_clone;
mod pr_comment;
mod pr_create;
#[cfg(test)]
mod test_helpers;
mod types;
mod url;

pub use api::{GitHubApi, RealGitHubApi};
pub use ci_status::CiStatus;
pub use client::GitHubClient;
pub use commit::Commit;
pub use git_clone::GitClone;
pub use pr_comment::PrComment;
pub use pr_create::PrCreate;
use types::{DiffComment, PrReviewsResponse, PullRequest};

use std::fmt::Write;
use std::sync::Arc;

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
    client: Arc<GitHubClient<A>>,
}

impl<A: GitHubApi> GitHub<A> {
    pub fn new(client: Arc<GitHubClient<A>>) -> Self {
        Self { client }
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
                Args::PrList { repo_dir, state } => self.pr_list(&repo_dir, state.as_deref()).await,
                Args::PrReviews {
                    repo_dir,
                    pr_number,
                } => self.pr_reviews(&repo_dir, pr_number).await,
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
}
