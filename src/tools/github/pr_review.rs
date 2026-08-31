//! `github_pr_review_submit` tool — submit a formal PR review.

use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{GithubApi, Tool, ToolCtx};
use crate::error::ToolError;
use crate::tools::string_or_value_required;

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
    #[serde(deserialize_with = "string_or_value_required")]
    line: u64,
    /// Comment body (Markdown).
    body: String,
}

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository directory relative to workspace root.
    repo_dir: String,
    /// PR number.
    #[serde(deserialize_with = "string_or_value_required")]
    pr_number: u64,
    /// Review summary and verdict (Markdown).
    body: String,
    /// APPROVE if the PR is sound, COMMENT otherwise.
    event: Event,
    /// Inline findings on the relevant diff lines.
    #[serde(default)]
    comments: Vec<InlineComment>,
}

pub struct PrReview(pub GithubApi);

impl Tool for PrReview {
    fn name(&self) -> &'static str {
        "github_pr_review_submit"
    }

    fn description(&self) -> &'static str {
        "Submit a formal PR review (APPROVE or COMMENT) with inline comments"
    }

    fn parameters(&self) -> serde_json::Value {
        crate::tools::schema_of::<Args>()
    }

    fn execute(
        &self,
        args: serde_json::Value,
        _ctx: ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let args: Args = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
            self.run(&args).await
        })
    }
}

impl PrReview {
    /// Pure: the create-review payload.
    fn payload(args: &Args) -> serde_json::Value {
        serde_json::json!({
            "body": args.body,
            "event": args.event,
            "comments": args.comments,
        })
    }

    async fn run(&self, args: &Args) -> Result<String, ToolError> {
        let nwo = self.0.nwo(&args.repo_dir).await?;
        let review = self
            .0
            .client()
            .create_review(&nwo, args.pr_number, &Self::payload(args))
            .await?;
        Ok(format!(
            "Review submitted on {nwo}#{}: {} (id {})",
            args.pr_number, review.state, review.id
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(event: Event, comments: Vec<InlineComment>) -> Args {
        Args {
            repo_dir: "projects/r".into(),
            pr_number: 141,
            body: "Looks solid.".into(),
            event,
            comments,
        }
    }

    #[test]
    fn payload_carries_body_event_and_comments() {
        let comments = vec![InlineComment {
            path: "src/lib.rs".into(),
            line: 10,
            body: "Off by one.".into(),
        }];
        let payload = PrReview::payload(&args(Event::Comment, comments));
        assert_eq!(payload["event"], "COMMENT");
        assert_eq!(payload["body"], "Looks solid.");
        assert_eq!(payload["comments"][0]["path"], "src/lib.rs");
        assert_eq!(payload["comments"][0]["line"], 10);
    }

    #[test]
    fn approve_serializes_uppercase_with_empty_comments() {
        let payload = PrReview::payload(&args(Event::Approve, vec![]));
        assert_eq!(payload["event"], "APPROVE");
        assert_eq!(payload["comments"], serde_json::json!([]));
    }

    #[test]
    fn request_changes_is_unrepresentable() {
        let result: Result<Event, _> = serde_json::from_str("\"REQUEST_CHANGES\"");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn submits_against_the_origin_repo() {
        let api = crate::tools::github::test_helpers::stub_api_with_repo(
            "owner/repo",
            |method, path, body| {
                assert_eq!(method, "POST");
                assert_eq!(path, "repos/owner/repo/pulls/141/reviews");
                let payload: serde_json::Value = serde_json::from_slice(&body.unwrap()).unwrap();
                assert_eq!(payload["event"], "APPROVE");
                br#"{"id":7,"state":"APPROVED"}"#.to_vec()
            },
        );
        let tool = PrReview(api);
        let out = tool.run(&args(Event::Approve, vec![])).await.unwrap();
        assert_eq!(out, "Review submitted on owner/repo#141: APPROVED (id 7)");
    }
}
