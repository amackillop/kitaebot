//! `git_clone` tool — clone a repository into the workspace.

use std::fmt::Write;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;
use tracing::debug;

use super::Tool;
use super::git_cli::GitCli;
use super::url::{extract_repo_name, to_https_url, validate_name};
use crate::error::ToolError;
use crate::tools::DirenvCache;
use crate::tools::cli_runner::{self, SubprocessCall};

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository URL (HTTPS or SSH). SSH URLs are rewritten to HTTPS
    /// automatically.
    url: String,
    /// Target directory name inside `projects/`. Defaults to the
    /// repository name derived from the URL.
    name: Option<String>,
}

pub struct GitClone(pub GitCli, pub DirenvCache);

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

        let mut output = self.0.exec_git(call, true).await?.format()?;
        let _ = write!(
            output,
            "\nCloned to projects/{repo_name} (use working_dir: \"projects/{repo_name}\" with exec)"
        );

        if target.join(".envrc").exists() {
            // Trust the .envrc synchronously so that any subsequent exec
            // call (which may race with the background warm) can already
            // run `direnv export json` successfully.
            direnv_allow(&target).await;

            warm_direnv_cache(self.1.clone(), target);
            let _ = write!(
                output,
                "\nDetected .envrc — warming direnv cache in the background. \
                 The devshell will be available shortly."
            );
        }

        Ok(output)
    }
}

/// Run `direnv allow` for a directory. Must complete before any
/// `direnv export json` call so the `.envrc` is trusted.
async fn direnv_allow(dir: &Path) {
    let call = SubprocessCall {
        binary: "direnv",
        args: vec!["allow".into()],
        cwd: dir.to_path_buf(),
        env: crate::tools::safe_env().collect(),
        timeout_secs: Some(10),
    };
    if let Err(e) = cli_runner::exec(&call).await {
        debug!(dir = %dir.display(), error = %e, "direnv allow failed");
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
        (GitClone(git, DirenvCache::new()), repo)
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
