//! `github_pr_diff_comments` tool — fetch inline code review comments.

use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;

use super::Tool;
use super::gh_cli::GhCli;
use super::types::DiffComment;
use crate::error::ToolError;
use crate::tools::cli_runner::SubprocessCall;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository directory relative to workspace root.
    repo_dir: String,
    /// PR number.
    pr_number: u64,
}

pub struct PrDiffComments(pub GhCli);

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
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let args: Args = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
            self.run(&args.repo_dir, args.pr_number).await
        })
    }
}

impl PrDiffComments {
    fn prepare(&self, repo_dir: &str, pr_number: u64) -> Result<SubprocessCall, ToolError> {
        let cwd = self.0.resolve_repo_dir(repo_dir)?;
        let endpoint = format!("repos/{{owner}}/{{repo}}/pulls/{pr_number}/comments");
        Ok(self.0.prepare_gh(&["api", &endpoint], &cwd))
    }

    /// Pure: format diff comments for display.
    fn format_output(comments: &[DiffComment]) -> String {
        if comments.is_empty() {
            return String::new();
        }

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
        let call = self.prepare(repo_dir, pr_number)?;
        let comments: Vec<DiffComment> = self.0.exec_parse(&call).await?;

        if comments.is_empty() {
            return Ok(format!("No inline comments on PR #{pr_number}."));
        }

        Ok(Self::format_output(&comments))
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::{Author, DiffComment};
    use super::*;
    use crate::tools::github::test_helpers::stub_gh_cli_with_repo;

    #[test]
    fn builds_correct_api_command() {
        let (gh, repo) = stub_gh_cli_with_repo();
        let tool = PrDiffComments(gh);
        let call = tool.prepare(&repo, 5).unwrap();
        assert_eq!(call.binary, "gh");
        assert_eq!(call.args[0], "api");
        assert!(call.args[1].contains("pulls/5/comments"));
    }

    #[test]
    fn formats_comments() {
        let comments = vec![
            DiffComment {
                id: 100,
                path: "src/main.rs".to_string(),
                line: Some(42),
                body: "Nit: rename this".to_string(),
                user: Author {
                    login: "alice".to_string(),
                },
            },
            DiffComment {
                id: 101,
                path: "src/lib.rs".to_string(),
                line: None,
                body: "Outdated".to_string(),
                user: Author {
                    login: "bob".to_string(),
                },
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

    #[test]
    fn empty_comments() {
        let result = PrDiffComments::format_output(&[]);
        assert!(result.is_empty());
    }
}
