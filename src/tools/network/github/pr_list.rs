//! `github_pr_list` tool — list pull requests.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;

use super::Tool;
use super::gh_cli::GhCli;
use super::types::PullRequest;
use crate::error::ToolError;
use crate::tools::cli_runner::CliRunner;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository directory relative to workspace root.
    repo_dir: String,
    /// Filter by state: `"open"` (default), `"closed"`, `"merged"`, `"all"`.
    state: Option<String>,
}

pub struct PrList<R>(pub Arc<GhCli<R>>);

impl<R: CliRunner> Tool for PrList<R> {
    fn name(&self) -> &'static str {
        "github_pr_list"
    }

    fn description(&self) -> &'static str {
        "List pull requests"
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
            self.run(&args.repo_dir, args.state.as_deref()).await
        })
    }
}

impl<R: CliRunner> PrList<R> {
    async fn run(&self, repo_dir: &str, state: Option<&str>) -> Result<String, ToolError> {
        let cwd = self.0.resolve_repo_dir(repo_dir)?;

        let state = state.unwrap_or("open");
        let valid_states = ["open", "closed", "merged", "all"];
        if !valid_states.contains(&state) {
            return Err(ToolError::InvalidArguments(format!(
                "invalid state: {state} (expected one of: {})",
                valid_states.join(", ")
            )));
        }

        let prs: Vec<PullRequest> = self
            .0
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
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::{ok_output, stub_gh_arc_with_repo};
    use super::*;

    #[test]
    fn deserialize_minimal() {
        let json = serde_json::json!({
            "repo_dir": "projects/myrepo"
        });
        let args: Args = serde_json::from_value(json).unwrap();
        assert_eq!(args.repo_dir, "projects/myrepo");
        assert!(args.state.is_none());
    }

    #[test]
    fn deserialize_with_state() {
        let json = serde_json::json!({
            "repo_dir": "projects/myrepo",
            "state": "closed"
        });
        let args: Args = serde_json::from_value(json).unwrap();
        assert_eq!(args.state.as_deref(), Some("closed"));
    }

    #[test]
    fn rejects_invalid_state() {
        let (client, repo) = stub_gh_arc_with_repo(vec![]);
        let tool = PrList(client);
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.run(&repo, Some("bogus")));
        assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
    }

    #[tokio::test]
    async fn formats_output() {
        let json = serde_json::to_string(&serde_json::json!([
            {"number": 1, "title": "Fix bug", "state": "OPEN", "url": "https://github.com/o/r/pull/1"},
            {"number": 2, "title": "Add feature", "state": "OPEN", "url": "https://github.com/o/r/pull/2"},
        ]))
        .unwrap();

        let (client, repo) = stub_gh_arc_with_repo(vec![ok_output(&json)]);
        let tool = PrList(client);
        let result = tool.run(&repo, None).await.unwrap();
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
    async fn empty_response() {
        let (client, repo) = stub_gh_arc_with_repo(vec![ok_output("[]")]);
        let tool = PrList(client);
        let result = tool.run(&repo, None).await.unwrap();
        assert_eq!(result, "No open pull requests.");
    }
}
