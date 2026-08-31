//! `git_fetch` tool — fetch refs from a remote.

use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;

use super::git_cli::GitCli;
use super::{Tool, ToolCtx};
use crate::error::ToolError;
use crate::tools::cli_runner::SubprocessCall;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository directory relative to workspace root
    /// (e.g. `"projects/myrepo"`).
    repo_dir: String,
    /// Remote name. Defaults to `"origin"`.
    remote: Option<String>,
    /// Branch or refspec to fetch. Defaults to the remote's configured
    /// refspecs, updating every tracking branch. Fetch a base branch
    /// before rebasing onto it.
    refspec: Option<String>,
}

pub struct Fetch(pub GitCli);

impl Tool for Fetch {
    fn name(&self) -> &'static str {
        "git_fetch"
    }

    fn description(&self) -> &'static str {
        "Fetch refs from a remote"
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
            self.run(
                &args.repo_dir,
                args.remote.as_deref(),
                args.refspec.as_deref(),
            )
            .await
        })
    }
}

impl Fetch {
    fn prepare(
        &self,
        repo_dir: &str,
        remote: Option<&str>,
        refspec: Option<&str>,
    ) -> Result<SubprocessCall, ToolError> {
        let cwd = self.0.resolve_repo_dir(repo_dir)?;
        let remote = remote.unwrap_or("origin");
        let mut args: Vec<&str> = vec!["fetch", remote];
        if let Some(r) = refspec {
            args.push(r);
        }
        Ok(self.0.prepare_git(&args, &cwd))
    }

    async fn run(
        &self,
        repo_dir: &str,
        remote: Option<&str>,
        refspec: Option<&str>,
    ) -> Result<String, ToolError> {
        let call = self.prepare(repo_dir, remote, refspec)?;
        self.0.exec_git(call, true).await?.format()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::git::test_helpers::stub_git_cli_with_repo;

    #[test]
    fn defaults_to_origin() {
        let (git, repo) = stub_git_cli_with_repo();
        let tool = Fetch(git);
        let call = tool.prepare(&repo, None, None).unwrap();
        assert_eq!(call.binary, "git");
        assert_eq!(call.args, ["fetch", "origin"]);
    }

    #[test]
    fn remote_and_refspec_build_correct_args() {
        let (git, repo) = stub_git_cli_with_repo();
        let tool = Fetch(git);
        let call = tool.prepare(&repo, Some("upstream"), Some("main")).unwrap();
        assert_eq!(call.args, ["fetch", "upstream", "main"]);
    }
}
