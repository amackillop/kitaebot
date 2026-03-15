//! `git_clone` tool — clone a repository into the workspace.

use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;

use super::Tool;
use super::git_cli::GitCli;
use super::url::{extract_repo_name, to_https_url, validate_name};
use crate::error::ToolError;
use crate::tools::cli_runner::SubprocessCall;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository URL (HTTPS or SSH). SSH URLs are rewritten to HTTPS
    /// automatically.
    url: String,
    /// Target directory name inside `projects/`. Defaults to the
    /// repository name derived from the URL.
    name: Option<String>,
}

pub struct GitClone(pub GitCli);

impl Tool for GitClone {
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

impl GitClone {
    /// Pure: validate args and build the git clone command.
    ///
    /// Does **not** check whether the target directory already exists
    /// (that's a filesystem effect handled by [`Self::run`]).
    fn prepare(&self, url: &str, name: Option<&str>) -> Result<SubprocessCall, ToolError> {
        let https_url = to_https_url(url)?;
        let repo_name = match name {
            Some(n) => validate_name(n)?.to_string(),
            None => extract_repo_name(&https_url)?,
        };

        let projects_dir = self.0.workspace_root().join("projects");
        Ok(self
            .0
            .prepare_git(&["clone", "--", &https_url, &repo_name], &projects_dir))
    }

    async fn run(&self, url: &str, name: Option<&str>) -> Result<String, ToolError> {
        let call = self.prepare(url, name)?;

        // Filesystem effects: check target doesn't exist, create projects dir.
        // repo_name is always the last arg: ["clone", "--", url, name]
        let repo_name = &call.args[3];
        let target = call.cwd.join(repo_name);

        if target.exists() {
            return Err(ToolError::ExecutionFailed(format!(
                "projects/{repo_name} already exists"
            )));
        }

        tokio::fs::create_dir_all(&call.cwd)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("mkdir projects/: {e}")))?;

        self.0.exec_git(call, true).await?.format()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::github::test_helpers::stub_git_cli_with_repo;

    #[test]
    fn builds_clone_command_with_derived_name() {
        let (git, _) = stub_git_cli_with_repo();
        let tool = GitClone(git);
        let call = tool
            .prepare("https://github.com/owner/repo.git", None)
            .unwrap();
        assert_eq!(call.binary, "git");
        assert_eq!(
            call.args,
            ["clone", "--", "https://github.com/owner/repo.git", "repo"]
        );
    }

    #[test]
    fn builds_clone_command_with_custom_name() {
        let (git, _) = stub_git_cli_with_repo();
        let tool = GitClone(git);
        let call = tool
            .prepare("https://github.com/owner/repo.git", Some("custom"))
            .unwrap();
        assert_eq!(call.args[3], "custom");
    }

    #[test]
    fn rewrites_ssh_to_https() {
        let (git, _) = stub_git_cli_with_repo();
        let tool = GitClone(git);
        let call = tool.prepare("git@github.com:owner/repo.git", None).unwrap();
        assert_eq!(call.args[2], "https://github.com/owner/repo.git");
    }

    #[test]
    fn rejects_traversal_in_name() {
        let (git, _) = stub_git_cli_with_repo();
        let tool = GitClone(git);
        let result = tool.prepare("https://github.com/owner/repo.git", Some("../escape"));
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rejects_already_existing_target() {
        let (git, _) = stub_git_cli_with_repo();
        // The stub already creates projects/r — clone into "r" to hit the exists check.
        let tool = GitClone(git);
        let result = tool.run("https://github.com/owner/r.git", None).await;
        assert!(
            matches!(result, Err(ToolError::ExecutionFailed(msg)) if msg.contains("already exists"))
        );
    }
}
