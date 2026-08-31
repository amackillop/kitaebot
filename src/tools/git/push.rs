//! `git_push` tool — push commits to a remote.
//!
//! Fast-forward only: published bot branches are append-only, so the
//! tool has no force option. Review feedback lands as new commits;
//! history restructuring, when wanted, is a human squash at merge.

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
    /// Branch to push. Resolved via `git symbolic-ref --short HEAD`
    /// when absent.
    branch: Option<String>,
    /// Set upstream tracking (`--set-upstream`).
    #[serde(default)]
    set_upstream: bool,
}

pub struct Push(pub GitCli);

impl Tool for Push {
    fn name(&self) -> &'static str {
        "git_push"
    }

    fn description(&self) -> &'static str {
        "Push commits to a remote (fast-forward only; published branches are append-only)"
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
                args.branch.as_deref(),
                args.set_upstream,
            )
            .await
        })
    }
}

impl Push {
    fn prepare(
        &self,
        repo_dir: &str,
        remote: Option<&str>,
        branch: Option<&str>,
        set_upstream: bool,
    ) -> Result<SubprocessCall, ToolError> {
        let cwd = self.0.resolve_repo_dir(repo_dir)?;
        let remote = remote.unwrap_or("origin");
        let mut args: Vec<&str> = vec!["push"];

        if set_upstream {
            args.push("--set-upstream");
        }
        args.push(remote);
        if let Some(b) = branch {
            args.push(b);
        }

        Ok(self.0.prepare_git(&args, &cwd))
    }

    async fn current_branch(&self, repo_dir: &str) -> Result<String, ToolError> {
        let cwd = self.0.resolve_repo_dir(repo_dir)?;
        let call = self
            .0
            .prepare_git(&["symbolic-ref", "--short", "HEAD"], &cwd);
        let out = self.0.exec_git(call, false).await?;
        if out.exit_code != 0 {
            return Err(ToolError::CommandFailed {
                command: "git symbolic-ref --short HEAD".into(),
                exit_code: out.exit_code,
                output: out.stderr.trim().to_string(),
            });
        }
        let branch = out.stdout.trim().to_string();
        if branch.is_empty() || branch == "HEAD" {
            return Err(ToolError::Precondition(
                "detached HEAD: no branch name to push".into(),
            ));
        }
        Ok(branch)
    }

    async fn run(
        &self,
        repo_dir: &str,
        remote: Option<&str>,
        branch: Option<&str>,
        set_upstream: bool,
    ) -> Result<String, ToolError> {
        let branch = match branch {
            Some(b) => Some(b.to_string()),
            None => Some(self.current_branch(repo_dir).await?),
        };
        let call = self.prepare(repo_dir, remote, branch.as_deref(), set_upstream)?;
        self.0.exec_git(call, true).await?.format()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::Secret;
    use crate::tools::DirenvCache;
    use crate::tools::git::test_helpers::stub_git_cli_with_repo;

    fn workspace_git() -> (GitCli, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let git = GitCli::new(
            Secret::test("fake"),
            dir.path(),
            DirenvCache::new(),
            Vec::new(),
        );
        (git, dir)
    }

    fn init_repo(dir: &std::path::Path, branch: &str) {
        for args in [
            &["init", "--initial-branch", branch][..],
            &["config", "user.email", "t@t"],
            &["config", "user.name", "t"],
            &["config", "commit.gpgsign", "false"],
            &["commit", "--allow-empty", "-m", "init"],
        ] {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    #[test]
    fn defaults_to_origin() {
        let (git, repo) = stub_git_cli_with_repo();
        let tool = Push(git);
        let call = tool.prepare(&repo, None, None, false).unwrap();
        assert_eq!(call.binary, "git");
        assert_eq!(call.args, ["push", "origin"]);
    }

    #[test]
    fn all_options_build_correct_args() {
        let (git, repo) = stub_git_cli_with_repo();
        let tool = Push(git);
        let call = tool
            .prepare(&repo, Some("upstream"), Some("feat"), true)
            .unwrap();
        assert_eq!(call.args, ["push", "--set-upstream", "upstream", "feat"]);
    }

    #[test]
    fn schema_has_no_force_option() {
        let (git, _repo) = stub_git_cli_with_repo();
        let schema = Push(git).parameters();
        assert!(
            schema["properties"].get("force").is_none(),
            "published branches are append-only; force must not come back quietly"
        );
    }

    #[tokio::test]
    async fn current_branch_resolves_local_name() {
        let (git, dir) = workspace_git();
        let repo_dir = dir.path().join("projects/r");
        std::fs::create_dir_all(&repo_dir).unwrap();
        init_repo(&repo_dir, "mybranch");
        let tool = Push(git);
        let branch = tool.current_branch("projects/r").await.unwrap();
        assert_eq!(branch, "mybranch");
    }

    #[tokio::test]
    async fn current_branch_rejects_detached_head() {
        let (git, dir) = workspace_git();
        let repo_dir = dir.path().join("projects/r");
        std::fs::create_dir_all(&repo_dir).unwrap();
        init_repo(&repo_dir, "main");
        // Detach HEAD
        let out = std::process::Command::new("git")
            .args(["checkout", "--detach"])
            .current_dir(&repo_dir)
            .output()
            .unwrap();
        assert!(out.status.success());
        let tool = Push(git);
        let err = tool.current_branch("projects/r").await.unwrap_err();
        assert!(matches!(err, ToolError::CommandFailed { .. }));
    }

    #[tokio::test]
    async fn run_resolves_branch_when_absent() {
        let (git, dir) = workspace_git();
        let repo_dir = dir.path().join("projects/r");
        std::fs::create_dir_all(&repo_dir).unwrap();
        init_repo(&repo_dir, "feat-branch");
        let tool = Push(git);
        // No remote configured, so the push fails — but the branch
        // resolution happens first, so the echoed command must carry
        // the resolved branch name, not a bare `git push origin`.
        let err = tool.run("projects/r", None, None, false).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("git push origin feat-branch"),
            "push should carry the resolved branch; got: {msg}"
        );
    }
}
