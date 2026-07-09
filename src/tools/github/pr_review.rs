//! `github_pr_review_submit` tool — submit a formal PR review.

use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::gh_cli::GhCli;
use super::{Tool, ToolCtx};
use crate::error::ToolError;
use crate::tools::cli_runner::{self, SubprocessCall};

/// Review verdict. `REQUEST_CHANGES` is deliberately unrepresentable:
/// blocking judgments stay with humans.
#[derive(Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum Event {
    /// The PR is sound.
    Approve,
    /// Findings to discuss; a critical finding is a COMMENT that says so.
    Comment,
}

/// An inline finding anchored to a diff line.
#[derive(Deserialize, Serialize, JsonSchema)]
struct InlineComment {
    /// File path relative to the repo root.
    path: String,
    /// Line number in the diff (right side).
    line: u64,
    /// Comment body (Markdown).
    body: String,
}

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository directory relative to workspace root.
    repo_dir: String,
    /// PR number.
    pr_number: u64,
    /// Review summary and verdict (Markdown).
    body: String,
    /// APPROVE if the PR is sound, COMMENT otherwise.
    event: Event,
    /// Inline findings on the relevant diff lines.
    #[serde(default)]
    comments: Vec<InlineComment>,
}

pub struct PrReview(pub GhCli);

impl Tool for PrReview {
    fn name(&self) -> &'static str {
        "github_pr_review_submit"
    }

    fn description(&self) -> &'static str {
        "Submit a formal PR review (APPROVE or COMMENT) with inline comments"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(Args)).expect("schema serialization failed")
    }

    fn execute(
        &self,
        args: serde_json::Value,
        _ctx: ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let args: Args = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
            let call = self.prepare(&args)?;
            cli_runner::exec(&call).await?.format()
        })
    }
}

impl PrReview {
    fn prepare(&self, args: &Args) -> Result<SubprocessCall, ToolError> {
        let cwd = self.0.resolve_repo_dir(&args.repo_dir)?;
        let endpoint = format!("repos/{{owner}}/{{repo}}/pulls/{}/reviews", args.pr_number);
        let payload = serde_json::json!({
            "body": args.body,
            "event": args.event,
            "comments": args.comments,
        })
        .to_string();
        let mut call = self.0.prepare_gh(
            &["api", "--method", "POST", &endpoint, "--input", "-"],
            &cwd,
        );
        call.stdin = Some(payload);
        Ok(call)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::github::test_helpers::stub_gh_cli_with_repo;

    fn args(repo: &str, event: Event, comments: Vec<InlineComment>) -> Args {
        Args {
            repo_dir: repo.into(),
            pr_number: 141,
            body: "Looks solid.".into(),
            event,
            comments,
        }
    }

    #[test]
    fn posts_review_payload_via_stdin() {
        let (gh, repo) = stub_gh_cli_with_repo();
        let tool = PrReview(gh);
        let comments = vec![InlineComment {
            path: "src/lib.rs".into(),
            line: 10,
            body: "Off by one.".into(),
        }];
        let call = tool
            .prepare(&args(&repo, Event::Comment, comments))
            .unwrap();
        assert_eq!(call.binary, "gh");
        assert_eq!(
            call.args,
            [
                "api",
                "--method",
                "POST",
                "repos/{owner}/{repo}/pulls/141/reviews",
                "--input",
                "-"
            ]
        );
        let payload: serde_json::Value =
            serde_json::from_str(call.stdin.as_deref().unwrap()).unwrap();
        assert_eq!(payload["event"], "COMMENT");
        assert_eq!(payload["body"], "Looks solid.");
        assert_eq!(payload["comments"][0]["path"], "src/lib.rs");
        assert_eq!(payload["comments"][0]["line"], 10);
        assert!(call.has_env("GH_TOKEN"));
    }

    #[test]
    fn approve_serializes_uppercase_with_empty_comments() {
        let (gh, repo) = stub_gh_cli_with_repo();
        let tool = PrReview(gh);
        let call = tool.prepare(&args(&repo, Event::Approve, vec![])).unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(call.stdin.as_deref().unwrap()).unwrap();
        assert_eq!(payload["event"], "APPROVE");
        assert_eq!(payload["comments"], serde_json::json!([]));
    }

    #[test]
    fn request_changes_is_unrepresentable() {
        let result: Result<Event, _> = serde_json::from_str("\"REQUEST_CHANGES\"");
        assert!(result.is_err());
    }
}
