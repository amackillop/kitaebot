//! `github_pr_create` tool — create a pull request.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;

use super::Tool;
use super::api::GitHubApi;
use super::client::GitHubClient;
use crate::error::ToolError;

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

pub struct PrCreate<A>(pub Arc<GitHubClient<A>>);

impl<A: GitHubApi> Tool for PrCreate<A> {
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

impl<A: GitHubApi> PrCreate<A> {
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
    use super::Args;

    #[test]
    fn deserialize_minimal() {
        let json = serde_json::json!({
            "repo_dir": "projects/myrepo",
            "title": "Fix bug",
            "body": "Fixes the thing"
        });
        let args: Args = serde_json::from_value(json).unwrap();
        assert_eq!(args.title, "Fix bug");
        assert_eq!(args.body, "Fixes the thing");
        assert!(args.base.is_none());
        assert!(!args.draft);
    }

    #[test]
    fn deserialize_full() {
        let json = serde_json::json!({
            "repo_dir": "projects/myrepo",
            "title": "Feature",
            "body": "Add feature",
            "base": "develop",
            "draft": true
        });
        let args: Args = serde_json::from_value(json).unwrap();
        assert_eq!(args.base.as_deref(), Some("develop"));
        assert!(args.draft);
    }
}
