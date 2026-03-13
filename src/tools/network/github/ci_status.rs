//! `github_ci_status` tool — fetch the latest failed CI run and its logs.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;

use super::Tool;
use super::gh_cli::GhCli;
use super::git_cli::GitCli;
use super::types::WorkflowRun;
use crate::error::ToolError;
use crate::tools::cli_runner::CliRunner;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository directory relative to workspace root
    /// (e.g. `"projects/myrepo"`).
    repo_dir: String,
    /// Branch to check. Defaults to the currently checked-out branch.
    branch: Option<String>,
}

pub struct CiStatus<R> {
    pub git: Arc<GitCli<R>>,
    pub gh: Arc<GhCli<R>>,
}

impl<R: CliRunner> Tool for CiStatus<R> {
    fn name(&self) -> &'static str {
        "github_ci_status"
    }

    fn description(&self) -> &'static str {
        "Fetch the latest failed CI run and its failure logs"
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
            self.run(&args.repo_dir, args.branch.as_deref()).await
        })
    }
}

impl<R: CliRunner> CiStatus<R> {
    async fn run(&self, repo_dir: &str, branch: Option<&str>) -> Result<String, ToolError> {
        let cwd = self.gh.resolve_repo_dir(repo_dir)?;

        let branch_name = match branch {
            Some(b) => b.to_string(),
            None => self.git.current_branch(&cwd).await?,
        };

        let runs: Vec<WorkflowRun> = self
            .gh
            .run_gh_json(
                &[
                    "run",
                    "list",
                    "--branch",
                    &branch_name,
                    "--status",
                    "failure",
                    "--limit",
                    "1",
                ],
                "databaseId,displayTitle,createdAt,url,workflowName",
                &cwd,
            )
            .await?;

        let run = runs.first().ok_or_else(|| {
            ToolError::ExecutionFailed(format!("no failed runs on branch `{branch_name}`"))
        })?;

        let id_str = run.database_id.to_string();
        let logs = self
            .gh
            .run_gh(&["run", "view", &id_str, "--log-failed"], &cwd)
            .await?;

        let mut output = format!(
            "Run #{}: \"{}\" ({})\n\
             Created: {}\n\
             URL: {}\n\n\
             ---\n\n",
            run.database_id, run.display_title, run.workflow_name, run.created_at, run.url
        );
        output.push_str(&logs);

        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::{ok_output, stub_gh_arc_with_repo, stub_git_arc_with_repo};
    use super::CiStatus;
    use crate::error::ToolError;

    #[test]
    fn deserialize_minimal() {
        let json = serde_json::json!({
            "repo_dir": "projects/myrepo"
        });
        let args: super::Args = serde_json::from_value(json).unwrap();
        assert_eq!(args.repo_dir, "projects/myrepo");
        assert!(args.branch.is_none());
    }

    #[test]
    fn deserialize_with_branch() {
        let json = serde_json::json!({
            "repo_dir": "projects/myrepo",
            "branch": "feature-xyz"
        });
        let args: super::Args = serde_json::from_value(json).unwrap();
        assert_eq!(args.branch.as_deref(), Some("feature-xyz"));
    }

    #[tokio::test]
    async fn formats_run_and_logs() {
        let runs_json = serde_json::to_string(&serde_json::json!([{
            "databaseId": 9999,
            "displayTitle": "CI",
            "createdAt": "2025-01-15T10:00:00Z",
            "url": "https://github.com/o/r/actions/runs/9999",
            "workflowName": "test"
        }]))
        .unwrap();

        let log_output = "test-job  Step failed";

        // Branch explicitly provided, so git stub is unused.
        let (git, _) = stub_git_arc_with_repo(vec![]);
        let (gh, repo) = stub_gh_arc_with_repo(vec![ok_output(&runs_json), ok_output(log_output)]);
        let tool = CiStatus { git, gh };
        let result = tool.run(&repo, Some("main")).await.unwrap();
        assert_eq!(
            result,
            "\
Run #9999: \"CI\" (test)
Created: 2025-01-15T10:00:00Z
URL: https://github.com/o/r/actions/runs/9999

---

$ stub
test-job  Step failed
Exit code: 0"
        );
    }

    #[tokio::test]
    async fn no_failed_runs() {
        let (git, _) = stub_git_arc_with_repo(vec![]);
        let (gh, repo) = stub_gh_arc_with_repo(vec![ok_output("[]")]);
        let tool = CiStatus { git, gh };
        let result = tool.run(&repo, Some("main")).await;
        assert!(
            matches!(result, Err(ToolError::ExecutionFailed(msg)) if msg.contains("no failed runs"))
        );
    }
}
