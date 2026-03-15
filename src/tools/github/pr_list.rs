//! `github_pr_list` tool — list pull requests.

use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;

use super::Tool;
use super::gh_cli::GhCli;
use super::types::PullRequest;
use crate::error::ToolError;
use crate::tools::cli_runner::SubprocessCall;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository directory relative to workspace root.
    repo_dir: String,
    /// Filter by state: `"open"` (default), `"closed"`, `"merged"`, `"all"`.
    state: Option<String>,
}

pub struct PrList(pub GhCli);

impl Tool for PrList {
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

impl PrList {
    fn prepare(&self, repo_dir: &str, state: &str) -> Result<SubprocessCall, ToolError> {
        let cwd = self.0.resolve_repo_dir(repo_dir)?;
        validate_state(state)?;
        Ok(self.0.prepare_gh(
            &[
                "pr",
                "list",
                "--state",
                state,
                "--json",
                "number,title,state,url",
            ],
            &cwd,
        ))
    }

    /// Pure: format pull requests for display.
    fn format_output(prs: &[PullRequest]) -> String {
        prs.iter()
            .map(|pr| format!("#{} {} [{}]\n  {}", pr.number, pr.title, pr.state, pr.url))
            .collect::<Vec<_>>()
            .join("\n")
    }

    async fn run(&self, repo_dir: &str, state: Option<&str>) -> Result<String, ToolError> {
        let state = state.unwrap_or("open");
        let call = self.prepare(repo_dir, state)?;
        let prs: Vec<PullRequest> = self.0.exec_parse(&call).await?;

        if prs.is_empty() {
            return Ok(format!("No {state} pull requests."));
        }

        Ok(Self::format_output(&prs))
    }
}

fn validate_state(state: &str) -> Result<(), ToolError> {
    let valid_states = ["open", "closed", "merged", "all"];
    if !valid_states.contains(&state) {
        return Err(ToolError::InvalidArguments(format!(
            "invalid state: {state} (expected one of: {})",
            valid_states.join(", ")
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::types::PullRequest;
    use super::*;
    use crate::tools::github::test_helpers::stub_gh_cli_with_repo;

    #[test]
    fn rejects_invalid_state() {
        let (gh, repo) = stub_gh_cli_with_repo();
        let tool = PrList(gh);
        let result = tool.prepare(&repo, "bogus");
        assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
    }

    #[test]
    fn builds_correct_list_command() {
        let (gh, repo) = stub_gh_cli_with_repo();
        let tool = PrList(gh);
        let call = tool.prepare(&repo, "open").unwrap();
        assert_eq!(call.binary, "gh");
        assert!(call.args.contains(&"--state".to_string()));
        assert!(call.args.contains(&"open".to_string()));
    }

    #[test]
    fn formats_prs() {
        let prs = vec![
            PullRequest {
                number: 1,
                title: "Fix bug".to_string(),
                state: "OPEN".to_string(),
                url: "https://github.com/o/r/pull/1".to_string(),
            },
            PullRequest {
                number: 2,
                title: "Add feature".to_string(),
                state: "OPEN".to_string(),
                url: "https://github.com/o/r/pull/2".to_string(),
            },
        ];
        let result = PrList::format_output(&prs);
        assert_eq!(
            result,
            "\
#1 Fix bug [OPEN]
  https://github.com/o/r/pull/1
#2 Add feature [OPEN]
  https://github.com/o/r/pull/2"
        );
    }
}
