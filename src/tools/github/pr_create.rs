//! `github_pr_create` tool — create a pull request.

use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;

use super::{GithubApi, Tool, ToolCtx, current_branch};
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

pub struct PrCreate(pub GithubApi);

impl Tool for PrCreate {
    fn name(&self) -> &'static str {
        "github_pr_create"
    }

    fn description(&self) -> &'static str {
        "Create a pull request"
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

impl PrCreate {
    /// Pure: the create-pull payload.
    fn payload(args: &Args, head: &str, base: &str) -> serde_json::Value {
        serde_json::json!({
            "title": args.title,
            "body": args.body,
            "head": head,
            "base": base,
            "draft": args.draft,
        })
    }

    async fn run(&self, args: &Args) -> Result<String, ToolError> {
        let dir = self.0.dir(&args.repo_dir)?;
        let nwo = self.0.nwo(&args.repo_dir).await?;
        // REST wants head and base spelled out where gh inferred them:
        // head is the checked-out branch, base the repo default.
        let head = current_branch(&dir).await?;
        let base = match &args.base {
            Some(base) => base.clone(),
            None => self.0.client().repo(&nwo).await?.default_branch,
        };
        let pull = self
            .0
            .client()
            .create_pull(&nwo, &Self::payload(args, &head, &base))
            .await?;
        Ok(format!("Created PR #{}: {}", pull.number, pull.html_url))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::github::test_helpers::stub_api_with_repo;

    fn args(base: Option<&str>, draft: bool) -> Args {
        Args {
            repo_dir: "projects/r".into(),
            title: "Fix bug".into(),
            body: "Fixes the thing".into(),
            base: base.map(String::from),
            draft,
        }
    }

    #[test]
    fn payload_names_head_and_base() {
        let payload = PrCreate::payload(&args(None, false), "feature", "main");
        assert_eq!(payload["title"], "Fix bug");
        assert_eq!(payload["head"], "feature");
        assert_eq!(payload["base"], "main");
        assert_eq!(payload["draft"], false);
    }

    #[tokio::test]
    async fn explicit_base_skips_the_repo_lookup() {
        let api = stub_api_with_repo("owner/repo", |method, path, body| {
            assert_eq!(method, "POST");
            assert_eq!(path, "repos/owner/repo/pulls");
            let payload: serde_json::Value = serde_json::from_slice(&body.unwrap()).unwrap();
            assert_eq!(payload["base"], "develop");
            assert_eq!(payload["draft"], true);
            br#"{"number":12,"html_url":"https://github.com/owner/repo/pull/12"}"#.to_vec()
        });
        // The stub repo has no commits, so seed a branch for HEAD.
        let tool = PrCreate(api);
        let out = tool.run(&args(Some("develop"), true)).await.unwrap();
        assert_eq!(out, "Created PR #12: https://github.com/owner/repo/pull/12");
    }
}
