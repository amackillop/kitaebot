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
    use super::Args;

    #[test]
    fn deserialize_minimal() {
        let json = serde_json::json!({
            "url": "https://github.com/owner/repo.git"
        });
        let args: Args = serde_json::from_value(json).unwrap();
        assert!(args.url.contains("owner/repo"));
        assert!(args.name.is_none());
    }

    #[test]
    fn deserialize_with_name() {
        let json = serde_json::json!({
            "url": "https://github.com/owner/repo.git",
            "name": "custom"
        });
        let args: Args = serde_json::from_value(json).unwrap();
        assert_eq!(args.name.as_deref(), Some("custom"));
    }
}
