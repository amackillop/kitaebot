//! `github_pr_create` tool — create a pull request.

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
    /// PR title.
    title: String,
    /// PR body / description.
    body: String,
    /// Base branch to merge into. Defaults to the repo's default branch.
    base: Option<String>,
    /// Create as draft PR.
    #[serde(default)]
    draft: bool,
}

pub struct PrCreate(pub GhCli);

impl Tool for PrCreate {
    fn name(&self) -> &'static str {
        "github_pr_create"
    }

    fn description(&self) -> &'static str {
        "Create a pull request"
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
            self.run(
                &args.repo_dir,
                &args.title,
                &args.body,
                args.base.as_deref(),
                args.draft,
            )
            .await
        })
    }
}

impl PrCreate {
    fn prepare(
        &self,
        repo_dir: &str,
        title: &str,
        body: &str,
        base: Option<&str>,
        draft: bool,
    ) -> Result<SubprocessCall, ToolError> {
        let cwd = self.0.resolve_repo_dir(repo_dir)?;

        let mut args = vec!["pr", "create", "--title", title, "--body", body];
        if let Some(b) = base {
            args.extend(["--base", b]);
        }
        if draft {
            args.push("--draft");
        }

        Ok(self.0.prepare_gh(&args, &cwd))
    }

    async fn run(
        &self,
        repo_dir: &str,
        title: &str,
        body: &str,
        base: Option<&str>,
        draft: bool,
    ) -> Result<String, ToolError> {
        let call = self.prepare(repo_dir, title, body, base, draft)?;
        cli_runner::exec(&call).await?.format()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::github::test_helpers::stub_gh_cli_with_repo;

    #[test]
    fn creates_pr_with_minimal_args() {
        let (gh, repo) = stub_gh_cli_with_repo();
        let tool = PrCreate(gh);
        let call = tool
            .prepare(&repo, "Fix bug", "Fixes the thing", None, false)
            .unwrap();
        assert_eq!(call.binary, "gh");
        assert_eq!(
            call.args,
            [
                "pr",
                "create",
                "--title",
                "Fix bug",
                "--body",
                "Fixes the thing"
            ]
        );
        assert!(call.has_env("GH_TOKEN"));
    }

    #[test]
    fn draft_with_base_appends_flags() {
        let (gh, repo) = stub_gh_cli_with_repo();
        let tool = PrCreate(gh);
        let call = tool
            .prepare(&repo, "Feature", "Add feature", Some("develop"), true)
            .unwrap();
        assert_eq!(
            call.args,
            [
                "pr",
                "create",
                "--title",
                "Feature",
                "--body",
                "Add feature",
                "--base",
                "develop",
                "--draft"
            ]
        );
    }
}
