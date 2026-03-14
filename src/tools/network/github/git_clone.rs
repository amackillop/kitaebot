//! `git_clone` tool — clone a repository into the workspace.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;

use super::Tool;
use super::git_cli::GitCli;
use super::url::{extract_repo_name, to_https_url, validate_name};
use crate::error::ToolError;
use crate::tools::cli_runner::CliRunner;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository URL (HTTPS or SSH). SSH URLs are rewritten to HTTPS
    /// automatically.
    url: String,
    /// Target directory name inside `projects/`. Defaults to the
    /// repository name derived from the URL.
    name: Option<String>,
}

pub struct GitClone<R>(pub Arc<GitCli<R>>);

impl<R: CliRunner> Tool for GitClone<R> {
    fn name(&self) -> &'static str {
        "git_clone"
    }

    fn description(&self) -> &'static str {
        "Clone a repository into the workspace"
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
            self.run(&args.url, args.name.as_deref()).await
        })
    }
}

impl<R: CliRunner> GitClone<R> {
    async fn run(&self, url: &str, name: Option<&str>) -> Result<String, ToolError> {
        let https_url = to_https_url(url)?;
        let repo_name = match name {
            Some(n) => validate_name(n)?.to_string(),
            None => extract_repo_name(&https_url)?,
        };

        let projects_dir = self.0.workspace_root().join("projects");
        let target = projects_dir.join(&repo_name);

        if target.exists() {
            return Err(ToolError::ExecutionFailed(format!(
                "projects/{repo_name} already exists"
            )));
        }

        tokio::fs::create_dir_all(&projects_dir)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("mkdir projects/: {e}")))?;

        self.0
            .run_git(
                &["clone", "--", &https_url, &repo_name],
                &projects_dir,
                true,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ToolError;
    use crate::tools::network::github::test_helpers::{ok_output, stub_git_arc_with_repo};

    #[tokio::test]
    async fn clones_with_derived_name_authenticated() {
        let (git, _, calls) = stub_git_arc_with_repo(vec![ok_output("Cloning into 'repo'...")]);
        let tool = GitClone(git);
        let _ = tool
            .run("https://github.com/owner/repo.git", None)
            .await
            .unwrap();

        let recorded = calls.take().await;
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].binary, "git");
        assert_eq!(
            recorded[0].args,
            ["clone", "--", "https://github.com/owner/repo.git", "repo"]
        );
        assert!(recorded[0].has_env("GIT_ASKPASS"));
    }

    #[tokio::test]
    async fn uses_custom_name() {
        let (git, _, calls) = stub_git_arc_with_repo(vec![ok_output("ok")]);
        let tool = GitClone(git);
        let _ = tool
            .run("https://github.com/owner/repo.git", Some("custom"))
            .await
            .unwrap();

        let recorded = calls.take().await;
        assert_eq!(recorded[0].args[3], "custom");
    }

    #[tokio::test]
    async fn rewrites_ssh_to_https() {
        let (git, _, calls) = stub_git_arc_with_repo(vec![ok_output("ok")]);
        let tool = GitClone(git);
        let _ = tool
            .run("git@github.com:owner/repo.git", None)
            .await
            .unwrap();

        let recorded = calls.take().await;
        assert_eq!(recorded[0].args[2], "https://github.com/owner/repo.git");
    }

    #[tokio::test]
    async fn rejects_already_existing_target() {
        let (git, _, _) = stub_git_arc_with_repo(vec![]);
        // The stub already creates projects/r — clone into "r" to hit the exists check.
        let tool = GitClone(git);
        let result = tool.run("https://github.com/owner/r.git", None).await;
        assert!(
            matches!(result, Err(ToolError::ExecutionFailed(msg)) if msg.contains("already exists"))
        );
    }

    #[tokio::test]
    async fn rejects_traversal_in_name() {
        let (git, _, _) = stub_git_arc_with_repo(vec![]);
        let tool = GitClone(git);
        let result = tool
            .run("https://github.com/owner/repo.git", Some("../escape"))
            .await;
        assert!(result.is_err());
    }
}
