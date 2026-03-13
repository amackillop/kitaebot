//! `git_push` tool — push commits to a remote.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;

use super::Tool;
use super::git_cli::GitCli;
use crate::error::ToolError;
use crate::tools::cli_runner::CliRunner;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository directory relative to workspace root
    /// (e.g. `"projects/myrepo"`).
    repo_dir: String,
    /// Remote name. Defaults to `"origin"`.
    remote: Option<String>,
    /// Branch to push. Defaults to the current branch.
    branch: Option<String>,
    /// Set upstream tracking (`--set-upstream`).
    #[serde(default)]
    set_upstream: bool,
}

pub struct Push<R>(pub Arc<GitCli<R>>);

impl<R: CliRunner> Tool for Push<R> {
    fn name(&self) -> &'static str {
        "git_push"
    }

    fn description(&self) -> &'static str {
        "Push commits to a remote"
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
                args.remote.as_deref(),
                args.branch.as_deref(),
                args.set_upstream,
            )
            .await
        })
    }
}

impl<R: CliRunner> Push<R> {
    async fn run(
        &self,
        repo_dir: &str,
        remote: Option<&str>,
        branch: Option<&str>,
        set_upstream: bool,
    ) -> Result<String, ToolError> {
        let cwd = self.0.resolve_repo_dir(repo_dir)?;

        let remote = remote.unwrap_or("origin");
        let mut args = vec!["push"];

        if set_upstream {
            args.push("--set-upstream");
        }

        args.push(remote);

        if let Some(b) = branch {
            args.push(b);
        }

        self.0.run_git(&args, &cwd, true).await
    }
}

#[cfg(test)]
mod tests {
    use super::Args;

    #[test]
    fn deserialize_minimal() {
        let json = serde_json::json!({
            "repo_dir": "projects/myrepo"
        });
        let args: Args = serde_json::from_value(json).unwrap();
        assert_eq!(args.repo_dir, "projects/myrepo");
        assert!(args.remote.is_none());
        assert!(args.branch.is_none());
        assert!(!args.set_upstream);
    }

    #[test]
    fn deserialize_full() {
        let json = serde_json::json!({
            "repo_dir": "projects/myrepo",
            "remote": "upstream",
            "branch": "feature",
            "set_upstream": true
        });
        let args: Args = serde_json::from_value(json).unwrap();
        assert_eq!(args.remote.as_deref(), Some("upstream"));
        assert_eq!(args.branch.as_deref(), Some("feature"));
        assert!(args.set_upstream);
    }
}
