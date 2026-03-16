//! `github_ci_status` tool — fetch the latest failed CI run and its logs.

use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;

use super::Tool;
use super::gh_cli::GhCli;
use super::types::WorkflowRun;
use crate::error::ToolError;
use crate::tools::cli_runner::{self, SubprocessCall};

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository directory relative to workspace root
    /// (e.g. `"projects/myrepo"`).
    repo_dir: String,
    /// Branch to check. Defaults to the currently checked-out branch.
    branch: Option<String>,
}

pub struct CiStatus(pub GhCli);

impl Tool for CiStatus {
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

/// Get the current branch name from a git working directory.
async fn current_branch(cwd: &std::path::Path) -> Result<String, ToolError> {
    let env = crate::tools::safe_env().collect();
    let call = SubprocessCall {
        binary: "git",
        args: vec!["rev-parse".into(), "--abbrev-ref".into(), "HEAD".into()],
        cwd: cwd.to_path_buf(),
        env,
        timeout_secs: None,
    };
    let output = cli_runner::exec(&call).await?;
    if output.exit_code != 0 {
        return Err(ToolError::ExecutionFailed(format!(
            "failed to get current branch: {}",
            output.stderr
        )));
    }
    Ok(output.stdout.trim().to_string())
}

impl CiStatus {
    /// Build the command to list failed runs on a branch.
    fn prepare_list_runs(&self, branch: &str, cwd: &std::path::Path) -> SubprocessCall {
        self.0.prepare_gh(
            &[
                "run",
                "list",
                "--branch",
                branch,
                "--status",
                "failure",
                "--limit",
                "1",
                "--json",
                "databaseId,displayTitle,createdAt,url,workflowName",
            ],
            cwd,
        )
    }

    /// Build the command to fetch failed logs for a run.
    fn prepare_view_logs(&self, run_id: &str, cwd: &std::path::Path) -> SubprocessCall {
        self.0
            .prepare_gh(&["run", "view", run_id, "--log-failed"], cwd)
    }

    /// Pure: format the final output.
    fn format_output(run: &WorkflowRun, logs: &str) -> String {
        format!(
            "Run #{}: \"{}\" ({})\n\
             Created: {}\n\
             URL: {}\n\n\
             ---\n\n\
             {}",
            run.database_id, run.display_title, run.workflow_name, run.created_at, run.url, logs
        )
    }

    async fn run(&self, repo_dir: &str, branch: Option<&str>) -> Result<String, ToolError> {
        let cwd = self.0.resolve_repo_dir(repo_dir)?;

        let branch_name = match branch {
            Some(b) => b.to_string(),
            None => current_branch(&cwd).await?,
        };

        let list_call = self.prepare_list_runs(&branch_name, &cwd);
        let runs: Vec<WorkflowRun> = self.0.exec_parse(&list_call).await?;

        let run = runs.first().ok_or_else(|| {
            ToolError::ExecutionFailed(format!("no failed runs on branch `{branch_name}`"))
        })?;

        let id_str = run.database_id.to_string();
        let logs_call = self.prepare_view_logs(&id_str, &cwd);
        let logs = cli_runner::exec(&logs_call).await?.format()?;

        Ok(Self::format_output(run, &logs))
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::WorkflowRun;
    use super::*;
    use crate::tools::github::test_helpers::stub_gh_cli_with_repo;

    #[test]
    fn prepare_list_runs_command() {
        let (gh, repo) = stub_gh_cli_with_repo();
        let cwd = gh.resolve_repo_dir(&repo).unwrap();
        let tool = CiStatus(gh);
        let call = tool.prepare_list_runs("main", &cwd);
        assert_eq!(call.binary, "gh");
        assert!(call.args.contains(&"main".to_string()));
        assert!(call.args.contains(&"failure".to_string()));
    }

    #[test]
    fn prepare_view_logs_command() {
        let (gh, repo) = stub_gh_cli_with_repo();
        let cwd = gh.resolve_repo_dir(&repo).unwrap();
        let tool = CiStatus(gh);
        let call = tool.prepare_view_logs("9999", &cwd);
        assert_eq!(call.binary, "gh");
        assert!(call.args.contains(&"9999".to_string()));
        assert!(call.args.contains(&"--log-failed".to_string()));
    }

    #[test]
    fn formats_run_and_logs() {
        let run = WorkflowRun {
            database_id: 9999,
            display_title: "CI".to_string(),
            created_at: "2025-01-15T10:00:00Z".to_string(),
            url: "https://github.com/o/r/actions/runs/9999".to_string(),
            workflow_name: "test".to_string(),
        };
        let result = CiStatus::format_output(&run, "test-job  Step failed");
        assert_eq!(
            result,
            "\
Run #9999: \"CI\" (test)
Created: 2025-01-15T10:00:00Z
URL: https://github.com/o/r/actions/runs/9999

---

test-job  Step failed"
        );
    }
}
