//! `github_ci_status` tool — report the latest CI run on a branch,
//! with failure logs when it failed.

use std::fmt::Write;
use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;

use super::{GithubApi, Tool, ToolCtx, current_branch};
use crate::clients::github::WorkflowRun;
use crate::error::ToolError;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository directory relative to workspace root
    /// (e.g. `"projects/myrepo"`).
    repo_dir: String,
    /// Branch to check. Defaults to the currently checked-out branch.
    branch: Option<String>,
}

pub struct CiStatus(pub GithubApi);

impl Tool for CiStatus {
    fn name(&self) -> &'static str {
        "github_ci_status"
    }

    fn description(&self) -> &'static str {
        "Report the latest CI run on a branch, with failure logs when it failed"
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
            self.run(&args.repo_dir, args.branch.as_deref()).await
        })
    }
}

impl CiStatus {
    /// Pure: format the run header and per-job failure logs.
    fn format_output(run: &WorkflowRun, logs: &[(String, String)]) -> String {
        let outcome = match &run.conclusion {
            None => run.status.clone(),
            Some(c) => c.clone(),
        };
        let mut out = format!(
            "Run #{}: \"{}\" ({})\nOutcome: {}\nCreated: {}\nURL: {}\n",
            run.id, run.display_title, run.name, outcome, run.created_at, run.html_url
        );
        for (job, log) in logs {
            let _ = write!(out, "\n--- job: {job} ---\n\n{log}");
        }
        out
    }

    async fn run(&self, repo_dir: &str, branch: Option<&str>) -> Result<String, ToolError> {
        let dir = self.0.dir(repo_dir)?;
        let nwo = self.0.nwo(repo_dir).await?;
        let branch_name = match branch {
            Some(b) => b.to_string(),
            None => current_branch(&dir).await?,
        };

        let client = self.0.client();
        // No runs is the answer, not a failure: the model asked what
        // CI did and CI never ran.
        let Some(run) = client.latest_run(&nwo, &branch_name).await? else {
            return Ok(format!("No workflow runs on branch `{branch_name}`."));
        };

        if run.conclusion.as_deref() != Some("failure") {
            return Ok(Self::format_output(&run, &[]));
        }

        // Whole-job logs, not gh's failed-steps slice: the extra lines
        // carry the context around the failure anyway.
        let jobs = client.run_jobs(&nwo, run.id).await?;
        let mut logs = Vec::new();
        for job in jobs
            .iter()
            .filter(|j| j.conclusion.as_deref() == Some("failure"))
        {
            let log = client.job_logs(&nwo, job.id).await?;
            logs.push((job.name.clone(), log));
        }

        Ok(Self::format_output(&run, &logs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::github::test_helpers::stub_api_with_repo;

    fn run_fixture(status: &str, conclusion: Option<&str>) -> WorkflowRun {
        WorkflowRun {
            id: 9999,
            display_title: "CI".to_string(),
            name: "test".to_string(),
            status: status.to_string(),
            conclusion: conclusion.map(str::to_string),
            created_at: "2025-01-15T10:00:00Z".to_string(),
            html_url: "https://github.com/o/r/actions/runs/9999".to_string(),
        }
    }

    #[test]
    fn formats_run_and_logs() {
        let run = run_fixture("completed", Some("failure"));
        let logs = vec![("build".to_string(), "Step failed".to_string())];
        let result = CiStatus::format_output(&run, &logs);
        assert_eq!(
            result,
            "\
Run #9999: \"CI\" (test)
Outcome: failure
Created: 2025-01-15T10:00:00Z
URL: https://github.com/o/r/actions/runs/9999

--- job: build ---

Step failed"
        );
    }

    /// A run without a conclusion reports its status, so pending is
    /// distinguishable from green and from never-ran.
    #[test]
    fn pending_run_reports_status() {
        let run = run_fixture("in_progress", None);
        let result = CiStatus::format_output(&run, &[]);
        assert!(result.contains("Outcome: in_progress"), "{result}");
    }

    #[tokio::test]
    async fn fetches_failed_jobs_logs_only() {
        let api = stub_api_with_repo("owner/repo", |method, path, _body| {
            assert_eq!(method, "GET");
            if path.starts_with("repos/owner/repo/actions/runs?") {
                assert!(path.contains("branch=work"));
                return br#"{"workflow_runs":[{"id":7,"display_title":"CI",
                    "name":"test","status":"completed","conclusion":"failure",
                    "created_at":"2025-01-15T10:00:00Z",
                    "html_url":"https://example.invalid/7"}]}"#
                    .to_vec();
            }
            if path == "repos/owner/repo/actions/runs/7/jobs?per_page=100" {
                return br#"{"jobs":[
                    {"id":1,"name":"build","conclusion":"failure"},
                    {"id":2,"name":"lint","conclusion":"success"}]}"#
                    .to_vec();
            }
            assert_eq!(path, "repos/owner/repo/actions/jobs/1/logs");
            b"boom at step 3".to_vec()
        });
        let tool = CiStatus(api);
        let out = tool.run("projects/r", None).await.unwrap();
        assert!(out.contains("Run #7"));
        assert!(out.contains("--- job: build ---"));
        assert!(out.contains("boom at step 3"));
        assert!(!out.contains("lint"));
    }

    /// CI never running answers the question; it must not read as the
    /// tool failing.
    #[tokio::test]
    async fn no_runs_is_a_result_not_an_error() {
        let api = stub_api_with_repo("owner/repo", |_method, path, _body| {
            assert!(path.starts_with("repos/owner/repo/actions/runs?"));
            br#"{"workflow_runs":[]}"#.to_vec()
        });
        let tool = CiStatus(api);
        let out = tool.run("projects/r", None).await.unwrap();
        assert_eq!(out, "No workflow runs on branch `work`.");
    }

    /// A non-failed latest run is reported without touching the jobs
    /// or logs endpoints.
    #[tokio::test]
    async fn green_run_skips_job_and_log_fetches() {
        let api = stub_api_with_repo("owner/repo", |_method, path, _body| {
            assert!(
                path.starts_with("repos/owner/repo/actions/runs?"),
                "unexpected call: {path}"
            );
            br#"{"workflow_runs":[{"id":8,"display_title":"CI",
                "name":"test","status":"completed","conclusion":"success",
                "created_at":"2025-01-15T10:00:00Z",
                "html_url":"https://example.invalid/8"}]}"#
                .to_vec()
        });
        let tool = CiStatus(api);
        let out = tool.run("projects/r", None).await.unwrap();
        assert!(out.contains("Outcome: success"), "{out}");
        assert!(!out.contains("--- job:"), "{out}");
    }

    /// A log exceeding the ceiling is tail-truncated: the output ends
    /// with the log's final lines (the diagnosis), not its first ones.
    #[tokio::test]
    async fn oversized_log_keeps_tail() {
        let ceiling = crate::tools::TOOL_OUTPUT_CEILING_BYTES;
        let setup = "x".repeat(ceiling * 2);
        let diagnosis = "error: the diagnosis is at the end\n";
        let full_log = format!("{setup}{diagnosis}");
        let api = stub_api_with_repo("owner/repo", move |_method, path, _body| {
            if path.starts_with("repos/owner/repo/actions/runs?") {
                return br#"{"workflow_runs":[{"id":7,"display_title":"CI",
                    "name":"test","status":"completed","conclusion":"failure",
                    "created_at":"2025-01-15T10:00:00Z",
                    "html_url":"https://example.invalid/7"}]}"#
                    .to_vec();
            }
            if path.ends_with("/jobs?per_page=100") {
                return br#"{"jobs":[
                    {"id":1,"name":"build","conclusion":"failure"}]}"#
                    .to_vec();
            }
            assert!(path.ends_with("/logs"));
            full_log.as_bytes().to_vec()
        });
        let tool = CiStatus(api);
        let out = tool.run("projects/r", None).await.unwrap();
        // The output ends with the diagnosis (tail kept).
        assert!(
            out.ends_with(diagnosis),
            "output should end with the diagnosis"
        );
        // The output starts with the truncation header (head dropped).
        assert!(out.contains("[truncated "), "truncation header missing");
        // The very first bytes of the log are gone (head was dropped).
        assert!(
            !out.starts_with('x'),
            "head of the log should have been truncated"
        );
    }
}
