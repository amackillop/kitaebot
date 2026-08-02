//! `github_pr_reviews` tool — fetch review verdicts and PR comments.

use std::fmt::Write;
use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;

use super::{GithubApi, Tool, ToolCtx, api_err};
use crate::clients::github::{IssueComment, PrReview, RequestedReviewers};
use crate::error::ToolError;

/// Fetch top-level review verdicts and PR conversation comments.
///
/// Returns review approvals/rejections and top-level PR comments only.
/// Does NOT return inline code comments on specific lines — use
/// `github_pr_diff_comments` for those.
#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository directory relative to workspace root.
    repo_dir: String,
    /// PR number.
    pr_number: u64,
}

pub struct PrReviews(pub GithubApi);

impl Tool for PrReviews {
    fn name(&self) -> &'static str {
        "github_pr_reviews"
    }

    fn description(&self) -> &'static str {
        "Fetch top-level review verdicts and PR conversation comments"
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
            self.run(&args.repo_dir, args.pr_number).await
        })
    }
}

impl PrReviews {
    /// Pure: format reviews, pending reviewers, and comments.
    fn format_output(
        reviews: &[PrReview],
        pending: &RequestedReviewers,
        comments: &[IssueComment],
        pr_number: u64,
    ) -> String {
        let mut output = String::new();

        let names: Vec<&str> = pending
            .users
            .iter()
            .map(|u| u.login.as_str())
            .chain(pending.teams.iter().map(|t| t.name.as_str()))
            .collect();
        if !names.is_empty() {
            output.push_str("Pending reviewers: ");
            output.push_str(&names.join(", "));
            output.push_str("\n\n");
        }

        for r in reviews {
            let _ = writeln!(
                output,
                "@{} {} ({})",
                r.user.login,
                r.state,
                r.submitted_at.as_deref().unwrap_or("pending"),
            );
            if let Some(body) = r.body.as_deref().filter(|b| !b.is_empty()) {
                let _ = writeln!(output, "{body}");
            }
            output.push('\n');
        }

        for c in comments {
            let _ = writeln!(output, "@{} ({})\n{}", c.user.login, c.created_at, c.body);
            output.push('\n');
        }

        if output.is_empty() {
            return format!("No reviews or comments on PR #{pr_number}.");
        }

        output
    }

    async fn run(&self, repo_dir: &str, pr_number: u64) -> Result<String, ToolError> {
        let nwo = self.0.nwo(repo_dir).await?;
        let client = self.0.client();
        let number = u32::try_from(pr_number)
            .map_err(|_| ToolError::InvalidArguments("PR number out of range".into()))?;
        let reviews = client
            .pull_reviews(&nwo, number)
            .await
            .map_err(|e| api_err(&e))?;
        let pending = client
            .requested_reviewers(&nwo, pr_number)
            .await
            .map_err(|e| api_err(&e))?;
        let comments = client
            .issue_comments(&nwo, number)
            .await
            .map_err(|e| api_err(&e))?;
        Ok(Self::format_output(
            &reviews, &pending, &comments, pr_number,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::github::{TeamRef, UserRef};

    fn user(login: &str) -> UserRef {
        UserRef {
            login: login.into(),
        }
    }

    #[test]
    fn formats_reviews_and_comments() {
        let reviews = vec![PrReview {
            user: user("alice"),
            body: Some("Looks good".into()),
            state: "APPROVED".into(),
            submitted_at: Some("2025-01-15T10:00:00Z".into()),
        }];
        let pending = RequestedReviewers {
            users: vec![user("bob")],
            teams: vec![TeamRef {
                name: "platform".into(),
            }],
        };
        let comments = vec![IssueComment {
            user: user("carol"),
            body: "What about edge cases?".into(),
            created_at: "2025-01-15T11:00:00Z".into(),
        }];
        let result = PrReviews::format_output(&reviews, &pending, &comments, 42);
        assert_eq!(
            result,
            "\
Pending reviewers: bob, platform

@alice APPROVED (2025-01-15T10:00:00Z)
Looks good

@carol (2025-01-15T11:00:00Z)
What about edge cases?

"
        );
    }

    #[test]
    fn empty() {
        let pending = RequestedReviewers {
            users: vec![],
            teams: vec![],
        };
        let result = PrReviews::format_output(&[], &pending, &[], 1);
        assert_eq!(result, "No reviews or comments on PR #1.");
    }
}
