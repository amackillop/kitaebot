//! `github_pr_comment` tool — add a comment to a pull request.

use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;

use super::Tool;
use super::gh_cli::GhCli;
use crate::error::ToolError;
use crate::tools::cli_runner::{self, SubprocessCall};

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository directory relative to workspace root.
    repo_dir: String,
    /// PR number.
    pr_number: u64,
    /// Comment body (Markdown).
    body: String,
}

pub struct PrComment(pub GhCli);

impl Tool for PrComment {
    fn name(&self) -> &'static str {
        "github_pr_comment"
    }

    fn description(&self) -> &'static str {
        "Add a comment to a pull request"
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
            self.run(&args.repo_dir, args.pr_number, &args.body).await
        })
    }
}

impl PrComment {
    fn prepare(
        &self,
        repo_dir: &str,
        pr_number: u64,
        body: &str,
    ) -> Result<SubprocessCall, ToolError> {
        let cwd = self.0.resolve_repo_dir(repo_dir)?;
        let number = pr_number.to_string();
        Ok(self
            .0
            .prepare_gh(&["pr", "comment", &number, "--body", body], &cwd))
    }

    async fn run(&self, repo_dir: &str, pr_number: u64, body: &str) -> Result<String, ToolError> {
        let call = self.prepare(repo_dir, pr_number, body)?;
        cli_runner::exec(&call).await?.format()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::github::test_helpers::stub_gh_cli_with_repo;

    #[test]
    fn builds_correct_comment_command() {
        let (gh, repo) = stub_gh_cli_with_repo();
        let tool = PrComment(gh);
        let call = tool.prepare(&repo, 7, "LGTM").unwrap();
        assert_eq!(call.binary, "gh");
        assert_eq!(call.args, ["pr", "comment", "7", "--body", "LGTM"]);
        assert!(call.has_env("GH_TOKEN"));
    }
}
