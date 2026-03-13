//! `github_pr_diff_comments` tool — fetch inline code review comments.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;

use super::Tool;
use super::client::GitHubClient;
use super::types::DiffComment;
use crate::error::ToolError;
use crate::tools::cli_runner::CliRunner;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository directory relative to workspace root.
    repo_dir: String,
    /// PR number.
    pr_number: u64,
}

pub struct PrDiffComments<R>(pub Arc<GitHubClient<R>>);

impl<R: CliRunner> Tool for PrDiffComments<R> {
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
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let args: Args = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
            self.run(&args.repo_dir, args.pr_number).await
        })
    }
}

impl<R: CliRunner> PrDiffComments<R> {
    async fn run(&self, repo_dir: &str, pr_number: u64) -> Result<String, ToolError> {
        let cwd = self.0.resolve_repo_dir(repo_dir)?;
        let endpoint = format!("repos/{{owner}}/{{repo}}/pulls/{pr_number}/comments");

        let comments: Vec<DiffComment> = self.0.run_gh_api(&endpoint, &cwd).await?;

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
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::{ok_output, stub_arc_with_repo};
    use super::*;

    #[test]
    fn deserialize() {
        let json = serde_json::json!({
            "repo_dir": "projects/myrepo",
            "pr_number": 99
        });
        let args: Args = serde_json::from_value(json).unwrap();
        assert_eq!(args.pr_number, 99);
    }

    #[tokio::test]
    async fn formats_output() {
        let json = serde_json::to_string(&serde_json::json!([
            {"id": 100, "path": "src/main.rs", "line": 42, "body": "Nit: rename this", "user": {"login": "alice"}},
            {"id": 101, "path": "src/lib.rs", "line": null, "body": "Outdated", "user": {"login": "bob"}}
        ]))
        .unwrap();

        let (client, repo) = stub_arc_with_repo(vec![ok_output(&json)]);
        let tool = PrDiffComments(client);
        let result = tool.run(&repo, 5).await.unwrap();
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
    async fn empty() {
        let (client, repo) = stub_arc_with_repo(vec![ok_output("[]")]);
        let tool = PrDiffComments(client);
        let result = tool.run(&repo, 5).await.unwrap();
        assert_eq!(result, "No inline comments on PR #5.");
    }
}
