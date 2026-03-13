//! `github_pr_comment` tool — add a comment to a pull request.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;

use super::Tool;
use super::api::GitHubApi;
use super::client::GitHubClient;
use crate::error::ToolError;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository directory relative to workspace root.
    repo_dir: String,
    /// PR number.
    pr_number: u64,
    /// Comment body (Markdown).
    body: String,
}

pub struct PrComment<A>(pub Arc<GitHubClient<A>>);

impl<A: GitHubApi> Tool for PrComment<A> {
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

impl<A: GitHubApi> PrComment<A> {
    async fn run(&self, repo_dir: &str, pr_number: u64, body: &str) -> Result<String, ToolError> {
        let cwd = self.0.resolve_repo_dir(repo_dir)?;
        let number = pr_number.to_string();
        self.0
            .run_gh(&["pr", "comment", &number, "--body", body], &cwd)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::Args;

    #[test]
    fn deserialize() {
        let json = serde_json::json!({
            "repo_dir": "projects/myrepo",
            "pr_number": 7,
            "body": "LGTM"
        });
        let args: Args = serde_json::from_value(json).unwrap();
        assert_eq!(args.pr_number, 7);
        assert_eq!(args.body, "LGTM");
    }
}
