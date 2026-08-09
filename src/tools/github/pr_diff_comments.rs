//! `github_pr_diff_comments` tool — fetch inline code review comments.

use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;

use super::{GithubApi, Tool, ToolCtx};
use crate::clients::github::DiffComment;
use crate::error::ToolError;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository directory relative to workspace root.
    repo_dir: String,
    /// PR number.
    pr_number: u64,
}

pub struct PrDiffComments(pub GithubApi);

impl Tool for PrDiffComments {
    fn name(&self) -> &'static str {
        "github_pr_diff_comments"
    }

    fn description(&self) -> &'static str {
        "Fetch inline code review comments on specific lines in the diff"
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

impl PrDiffComments {
    /// Pure: format diff comments for display.
    fn format_output(comments: &[DiffComment]) -> String {
        comments
            .iter()
            .map(|c| {
                let location = c.line.map_or(c.path.clone(), |l| format!("{}:{l}", c.path));
                format!(
                    "[id:{}] @{} at {}\n{}",
                    c.id, c.user.login, location, c.body
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    async fn run(&self, repo_dir: &str, pr_number: u64) -> Result<String, ToolError> {
        let nwo = self.0.nwo(repo_dir).await?;
        let number = u32::try_from(pr_number)
            .map_err(|_| ToolError::InvalidArguments("PR number out of range".into()))?;
        let comments = self.0.client().pull_comments(&nwo, number).await?;

        if comments.is_empty() {
            return Ok(format!("No inline comments on PR #{pr_number}."));
        }

        Ok(Self::format_output(&comments))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::github::UserRef;
    use crate::tools::github::test_helpers::stub_api_with_repo;

    #[test]
    fn formats_comments() {
        let comments = vec![
            DiffComment {
                id: 100,
                path: "src/main.rs".to_string(),
                line: Some(42),
                body: "Nit: rename this".to_string(),
                user: UserRef {
                    login: "alice".to_string(),
                },
                created_at: "2025-01-15T10:00:00Z".to_string(),
            },
            DiffComment {
                id: 101,
                path: "src/lib.rs".to_string(),
                line: None,
                body: "Outdated".to_string(),
                user: UserRef {
                    login: "bob".to_string(),
                },
                created_at: "2025-01-15T10:00:00Z".to_string(),
            },
        ];
        let result = PrDiffComments::format_output(&comments);
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
    async fn fetches_from_the_origin_repo() {
        let api = stub_api_with_repo("owner/repo", |method, path, _body| {
            assert_eq!(method, "GET");
            assert_eq!(path, "repos/owner/repo/pulls/5/comments?per_page=100");
            b"[]".to_vec()
        });
        let tool = PrDiffComments(api);
        let out = tool.run("projects/r", 5).await.unwrap();
        assert_eq!(out, "No inline comments on PR #5.");
    }
}
