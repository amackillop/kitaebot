//! `git_clone` tool — clone a repository into the workspace.

use std::fmt::Write;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;
use tracing::debug;

use super::git_cli::GitCli;
use super::url::{extract_nwo, extract_repo_name, is_trusted_repo, to_https_url, validate_name};
use super::{Tool, ToolCtx};
use crate::error::ToolError;
use crate::tools::cli_runner::SubprocessCall;
use crate::tools::{DirenvCache, direnv};

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository URL (HTTPS or SSH). SSH URLs are rewritten to HTTPS
    /// automatically.
    url: String,
    /// Target directory name inside `projects/`. Defaults to the
    /// repository name derived from the URL.
    name: Option<String>,
}

pub struct GitClone {
    pub git: GitCli,
}

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
        _ctx: ToolCtx,
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

        let projects_dir = self.git.workspace_root().join("projects");
        Ok(self
            .git
            .prepare_git(&["clone", "--", &https_url, &repo_name], &projects_dir))
    }

    async fn run(&self, url: &str, name: Option<&str>) -> Result<String, ToolError> {
        let call = self.prepare(url, name)?;

        // Filesystem effects: check target doesn't exist, create projects dir.
        // repo_name is always the last arg: ["clone", "--", url, name]
        let repo_name = call.args[3].clone();
        let target = call.cwd.join(&repo_name);

        if target.exists() {
            return Err(ToolError::ExecutionFailed(format!(
                "projects/{repo_name} already exists"
            )));
        }

        tokio::fs::create_dir_all(&call.cwd)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("mkdir projects/: {e}")))?;

        // repo_name is derived from the URL or user-provided; the nwo for
        // the trust check always comes from the normalized clone URL.
        let nwo = extract_nwo(&call.args[2]);

        let mut output = self.git.exec_git(call, true).await?.format()?;
        let _ = write!(
            output,
            "\nCloned to projects/{repo_name} (use working_dir: \"projects/{repo_name}\" with exec)"
        );

        if target.join(".envrc").exists() {
            // An .envrc is arbitrary shell executing at clone time, before
            // anyone has read the repo. Only trust it for allowlisted
            // repos; an unresolvable nwo counts as untrusted.
            let trusted = nwo
                .as_deref()
                .is_some_and(|n| is_trusted_repo(n, &self.git.trusted_repos));
            if trusted {
                // Trust the .envrc synchronously so that any subsequent exec
                // call (which may race with the background warm) can already
                // run `direnv export json` successfully.
                direnv::allow(&target).await;

                warm_direnv_cache(self.git.direnv_cache.clone(), target);
                let _ = write!(
                    output,
                    "\nDetected .envrc — warming direnv cache in the background. \
                     The devshell will be available shortly."
                );
            } else {
                let _ = write!(
                    output,
                    "\n.envrc detected but {} is not in git.trusted_repos; \
                     direnv/devshell disabled for this clone.",
                    nwo.as_deref().unwrap_or("the repo")
                );
            }
        }

        Ok(output)
    }
}

/// Spawn a background task to pre-populate the shared direnv cache so the
/// first `exec` call in this directory is fast. `direnv allow` must have
/// already been run for the directory.
fn warm_direnv_cache(cache: DirenvCache, repo_dir: PathBuf) {
    tokio::spawn(async move {
        debug!(dir = %repo_dir.display(), "Warming direnv cache");
        match cache.get(&repo_dir).await {
            Ok(Some(_)) => debug!(dir = %repo_dir.display(), "Direnv cache warmed"),
            Ok(None) => debug!(dir = %repo_dir.display(), "No .envrc found"),
            Err(e) => debug!(dir = %repo_dir.display(), error = %e, "Direnv cache warming failed"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::git::test_helpers::stub_git_cli_with_repo;

    fn stub_clone() -> (GitClone, String) {
        let (git, repo) = stub_git_cli_with_repo();
        let tool = GitClone { git };
        (tool, repo)
    }

    #[test]
    fn builds_clone_command_with_derived_name() {
        let (tool, _) = stub_clone();
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
        let (tool, _) = stub_clone();
        let call = tool
            .prepare("https://github.com/owner/repo.git", Some("custom"))
            .unwrap();
        assert_eq!(call.args[3], "custom");
    }

    #[test]
    fn rewrites_ssh_to_https() {
        let (tool, _) = stub_clone();
        let call = tool.prepare("git@github.com:owner/repo.git", None).unwrap();
        assert_eq!(call.args[2], "https://github.com/owner/repo.git");
    }

    #[test]
    fn rejects_traversal_in_name() {
        let (tool, _) = stub_clone();
        let result = tool.prepare("https://github.com/owner/repo.git", Some("../escape"));
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rejects_already_existing_target() {
        let (tool, _) = stub_clone();
        // The stub already creates projects/r — clone into "r" to hit the exists check.
        let result = tool.run("https://github.com/owner/r.git", None).await;
        assert!(
            matches!(result, Err(ToolError::ExecutionFailed(msg)) if msg.contains("already exists"))
        );
    }
}
