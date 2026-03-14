//! `github_pr_create` tool — create a pull request.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;

use super::Tool;
use super::gh_cli::GhCli;
use crate::error::ToolError;
use crate::tools::cli_runner::CliRunner;

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

pub struct PrCreate<R>(pub Arc<GhCli<R>>);

impl<R: CliRunner> Tool for PrCreate<R> {
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

impl<R: CliRunner> PrCreate<R> {
    async fn run(
        &self,
        repo_dir: &str,
        title: &str,
        body: &str,
        base: Option<&str>,
        draft: bool,
    ) -> Result<String, ToolError> {
        let cwd = self.0.resolve_repo_dir(repo_dir)?;

        let mut args = vec!["pr", "create", "--title", title, "--body", body];

        if let Some(b) = base {
            args.extend(["--base", b]);
        }
        if draft {
            args.push("--draft");
        }

        self.0.run_gh(&args, &cwd).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::github::test_helpers::{ok_output, stub_gh_arc_with_repo};

    #[tokio::test]
    async fn creates_pr_with_minimal_args() {
        let (gh, repo, calls) =
            stub_gh_arc_with_repo(vec![ok_output("https://github.com/o/r/pull/42")]);
        let tool = PrCreate(gh);
        let _ = tool
            .run(&repo, "Fix bug", "Fixes the thing", None, false)
            .await
            .unwrap();

        let recorded = calls.take().await;
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].binary, "gh");
        assert_eq!(
            recorded[0].args,
            [
                "pr",
                "create",
                "--title",
                "Fix bug",
                "--body",
                "Fixes the thing"
            ]
        );
        assert!(recorded[0].has_env("GH_TOKEN"));
    }

    #[tokio::test]
    async fn draft_with_base_appends_flags() {
        let (gh, repo, calls) = stub_gh_arc_with_repo(vec![ok_output("ok")]);
        let tool = PrCreate(gh);
        let _ = tool
            .run(&repo, "Feature", "Add feature", Some("develop"), true)
            .await
            .unwrap();

        let recorded = calls.take().await;
        assert_eq!(
            recorded[0].args,
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
