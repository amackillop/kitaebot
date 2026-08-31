//! `github_issue_create` tool — open an issue.
//!
//! The write path for self-directed work (spec 24): discovery duties
//! file their proposals as issues. Created issues carry no assignee —
//! assignment is the human gate that lets the issues channel dispatch
//! work, so the bot can never trigger itself.

use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;

use super::{GithubApi, Tool, ToolCtx};
use crate::error::ToolError;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Target repository as `owner/repo`. Must be a configured
    /// repository.
    repo: String,
    /// Issue title.
    title: String,
    /// Issue body / description.
    body: String,
    /// Labels to apply: `bug` or `enhancement`, one of `priority:high`
    /// (burning money, turns, or data now) or `priority:low`, and
    /// `needs-plan` when the fix needs a design agreed before code.
    #[serde(default)]
    labels: Vec<String>,
}

pub struct IssueCreate {
    pub api: GithubApi,
    /// Repos the bot may file issues in — the `[git.repositories]` keys.
    pub repos: Vec<String>,
}

impl Tool for IssueCreate {
    fn name(&self) -> &'static str {
        "github_issue_create"
    }

    fn description(&self) -> &'static str {
        "Open a GitHub issue in a configured repository. The issue is \
         created unassigned; a human assigns it to pick it up as work."
    }

    fn parameters(&self) -> serde_json::Value {
        crate::tools::schema_of::<Args>()
    }

    fn execute(
        &self,
        args: serde_json::Value,
        _ctx: ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let args: Args = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
            self.run(&args).await
        })
    }
}

impl IssueCreate {
    async fn run(&self, args: &Args) -> Result<String, ToolError> {
        if !self
            .repos
            .iter()
            .any(|r| r.eq_ignore_ascii_case(&args.repo))
        {
            return Err(ToolError::Blocked {
                operation: format!("github_issue_create in {}", args.repo),
                guidance: format!(
                    "not a configured repository; issues can be filed in: {}",
                    self.repos.join(", ")
                ),
            });
        }
        let issue = self
            .api
            .client()
            .create_issue(&args.repo, &args.title, &args.body, &args.labels)
            .await?;
        Ok(format!(
            "Created issue #{}: {}",
            issue.number, issue.html_url
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::github::test_helpers::stub_api;

    fn tool(api: GithubApi) -> IssueCreate {
        IssueCreate {
            api,
            repos: vec!["owner/repo".into()],
        }
    }

    fn args(repo: &str) -> Args {
        Args {
            repo: repo.into(),
            title: "Flaky test".into(),
            body: "Details".into(),
            labels: vec!["bug".into(), "priority:low".into()],
        }
    }

    #[tokio::test]
    async fn creates_issue_in_configured_repo() {
        let api = stub_api(|method, path, body| {
            assert_eq!(method, "POST");
            assert_eq!(path, "repos/owner/repo/issues");
            let payload: serde_json::Value = serde_json::from_slice(&body.unwrap()).unwrap();
            assert_eq!(payload["title"], "Flaky test");
            assert_eq!(payload["body"], "Details");
            assert_eq!(payload["labels"][0], "bug");
            assert_eq!(payload["labels"][1], "priority:low");
            br#"{"number":42,"html_url":"https://github.com/owner/repo/issues/42"}"#.to_vec()
        });

        let out = tool(api).run(&args("owner/repo")).await.unwrap();
        assert_eq!(
            out,
            "Created issue #42: https://github.com/owner/repo/issues/42"
        );
    }

    #[tokio::test]
    async fn repo_matching_is_case_insensitive() {
        let api = stub_api(|_, _, _| {
            br#"{"number":1,"html_url":"https://github.com/owner/repo/issues/1"}"#.to_vec()
        });

        tool(api).run(&args("Owner/Repo")).await.unwrap();
    }

    #[tokio::test]
    async fn unconfigured_repo_is_blocked() {
        let api = stub_api(|_, _, _| panic!("must not reach the API"));

        let err = tool(api).run(&args("evil/repo")).await.unwrap_err();
        match err {
            ToolError::Blocked {
                operation,
                guidance,
            } => {
                assert!(operation.contains("evil/repo"));
                assert!(guidance.contains("owner/repo"));
            }
            other => panic!("expected Blocked, got: {other}"),
        }
    }
}
