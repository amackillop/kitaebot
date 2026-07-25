//! `git` subprocess wrapper.
//!
//! [`GitCli`] owns the token and workspace root needed by git tools
//! (clone, push, commit). Auth uses a temporary `GIT_ASKPASS` script
//! written to a private directory for the duration of one command.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use tracing::{debug, warn};

use crate::error::ToolError;
use crate::secrets::Secret;
use crate::tools::cli_runner::{self, CmdOutput, SubprocessCall};
use crate::tools::{DirenvCache, direnv};

/// Shared context for git tools.
#[derive(Clone)]
pub struct GitCli {
    pub(super) token: Secret,
    pub(super) workspace_root: PathBuf,
    pub(super) direnv_cache: DirenvCache,
    /// Repos (`owner/repo` or `owner/*`) whose `.envrc` may be trusted.
    pub(super) trusted_repos: Vec<String>,
}

impl GitCli {
    pub fn new(
        token: Secret,
        workspace_root: impl Into<PathBuf>,
        direnv_cache: DirenvCache,
        trusted_repos: Vec<String>,
    ) -> Self {
        Self {
            token,
            workspace_root: workspace_root.into(),
            direnv_cache,
            trusted_repos,
        }
    }

    /// Trust and warm the devShell of a prepared checkout.
    ///
    /// No-op without an `.envrc` or when origin is not in
    /// `trusted_repos`. Best-effort: a failed warm degrades to
    /// no-devshell, which exec already tolerates.
    pub async fn warm_devshell(&self, dir: &Path) {
        if !dir.join(".envrc").exists() {
            return;
        }
        if !super::origin_trusted(dir, &self.trusted_repos).await {
            debug!(dir = %dir.display(), "origin not in trusted_repos; skipping devshell warm");
            return;
        }
        direnv::allow(dir).await;
        if let Err(e) = self.direnv_cache.get(dir).await {
            warn!(dir = %dir.display(), error = %e, "devshell warm failed");
        }
    }

    /// Resolve and validate a repo directory within the workspace.
    pub fn resolve_repo_dir(&self, repo_dir: &str) -> Result<PathBuf, ToolError> {
        super::resolve_repo_dir(&self.workspace_root, repo_dir)
    }

    /// Workspace root path. Used by `GitClone` to locate the
    /// `projects/` directory.
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Build a [`SubprocessCall`] for `git` without executing it.
    ///
    /// The returned call does **not** include `GIT_ASKPASS` — that is
    /// an effect created at execution time by [`Self::exec_git`].
    #[allow(clippy::unused_self)] // method for API consistency with prepare_gh
    pub fn prepare_git(&self, args: &[&str], cwd: &Path) -> SubprocessCall {
        let env: Vec<(OsString, OsString)> = crate::tools::safe_env().collect();
        SubprocessCall {
            binary: "git",
            args: args.iter().map(ToString::to_string).collect(),
            cwd: cwd.to_path_buf(),
            env,
            timeout_secs: None,
            stdin: None,
        }
    }

    /// The SHA of the remote default-branch head, via `git ls-remote`.
    /// Needs no checkout — the duty scheduler's new-commits gate probes
    /// repos the bot may never have cloned.
    pub async fn remote_head(&self, nwo: &str) -> Result<String, ToolError> {
        let url = format!("https://github.com/{nwo}.git");
        let call = self.prepare_git(&["ls-remote", &url, "HEAD"], &self.workspace_root);
        let out = self.exec_git(call, true).await?;
        if out.exit_code != 0 {
            return Err(ToolError::ExecutionFailed(format!(
                "ls-remote {nwo} exited {}: {}",
                out.exit_code,
                out.stderr.trim(),
            )));
        }
        parse_ls_remote_head(&out.stdout).ok_or_else(|| {
            ToolError::ExecutionFailed(format!("ls-remote {nwo}: no HEAD in output"))
        })
    }

    /// Execute a [`SubprocessCall`] with optional credential injection.
    ///
    /// When `authenticated` is true, a temporary `GIT_ASKPASS` script
    /// is created, added to the call's env, and deleted after execution.
    pub async fn exec_git(
        &self,
        mut call: SubprocessCall,
        authenticated: bool,
    ) -> Result<CmdOutput, ToolError> {
        // Inject direnv devshell env so git hooks can find tools like `just`.
        match self.direnv_cache.get(&call.cwd).await {
            Ok(Some(ref env)) => {
                call.env
                    .extend(env.iter().map(|(k, v)| (k.into(), v.into())));
            }
            Ok(None) => {}
            Err(ref e) => {
                warn!(dir = %call.cwd.display(), error = %e, "direnv failed, running git without devshell");
            }
        }

        let askpass = if authenticated {
            Some(AskPass::create(&self.token).await?)
        } else {
            None
        };

        if let Some(ref ap) = askpass {
            call.env
                .push(("GIT_ASKPASS".into(), ap.path().as_os_str().to_owned()));
            call.env.push(("GIT_TERMINAL_PROMPT".into(), "0".into()));
        }

        let output = cli_runner::exec(&call).await;
        drop(askpass);
        output
    }
}

/// Extract the SHA from `git ls-remote <url> HEAD` output
/// (`"<sha>\tHEAD"`).
fn parse_ls_remote_head(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .find(|l| l.trim_end().ends_with("HEAD"))
        .and_then(|l| l.split_whitespace().next())
        .map(str::to_string)
}

// ── GIT_ASKPASS helper ──────────────────────────────────────────────

/// A temporary `GIT_ASKPASS` script that prints the token.
///
/// The script lives in a private temp directory (mode 0700). The
/// directory is owned by a `TempDir` and removed on drop, so cleanup
/// happens even if the git command fails or the future is cancelled.
struct AskPass {
    /// Path to the executable script inside `_dir`.
    path: PathBuf,
    /// Owns the temp directory. Removed on drop.
    _dir: tempfile::TempDir,
}

impl AskPass {
    async fn create(token: &Secret) -> Result<Self, ToolError> {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::Builder::new()
            .prefix("kitaebot-askpass-")
            .tempdir()
            .map_err(|e| ToolError::ExecutionFailed(format!("tmpdir: {e}")))?;

        let path = dir.path().join("askpass");
        let script = format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", token.expose());

        tokio::fs::write(&path, &script)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("write askpass: {e}")))?;

        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("chmod askpass: {e}")))?;

        Ok(Self { path, _dir: dir })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::parse_ls_remote_head;
    use crate::tools::git::test_helpers::stub_git_cli_with_repo;

    #[test]
    fn parse_ls_remote_head_extracts_sha() {
        let out = "8725e54c1b2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f\tHEAD\n";
        assert_eq!(
            parse_ls_remote_head(out).as_deref(),
            Some("8725e54c1b2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f")
        );
    }

    #[test]
    fn parse_ls_remote_head_ignores_other_refs() {
        let out = "aaaa\trefs/heads/main\n";
        assert_eq!(parse_ls_remote_head(out), None);
        assert_eq!(parse_ls_remote_head(""), None);
    }

    #[test]
    fn prepare_git_builds_correct_call() {
        let (cli, repo) = stub_git_cli_with_repo();
        let cwd = cli.resolve_repo_dir(&repo).unwrap();
        let call = cli.prepare_git(&["rev-parse", "--abbrev-ref", "HEAD"], &cwd);
        assert_eq!(call.binary, "git");
        assert_eq!(call.args, ["rev-parse", "--abbrev-ref", "HEAD"]);
        assert!(!call.has_env("GIT_ASKPASS"));
    }
}
