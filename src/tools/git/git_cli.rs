//! `git` subprocess wrapper.
//!
//! [`GitCli`] owns the token and workspace root needed by git tools
//! (clone, push, commit). Auth uses a temporary `GIT_ASKPASS` script
//! written under `state/askpass/` for the duration of one command:
//! the exec Landlock tier denies `state/`, so exec children cannot
//! read the token during the git window, while the git tier grants
//! the helper read + execute (spec 15).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracing::{debug, warn};

use crate::error::ToolError;
use crate::sandbox::Tier;
use crate::secrets::Secret;
use crate::tools::cli_runner::{self, CmdOutput, Confinement, SubprocessCall};
use crate::tools::warm::Warmer;
use crate::tools::{DirenvCache, direnv};

/// Shared context for git tools.
#[derive(Clone)]
pub struct GitCli {
    pub(super) token: Secret,
    pub(super) workspace_root: PathBuf,
    pub(super) direnv_cache: DirenvCache,
    /// Repos (`owner/repo`) whose `.envrc` may be trusted.
    pub(super) trusted_repos: Vec<String>,
    warmer: Warmer,
    /// Build-warm command per exact `owner/repo` (spec 03).
    warm_commands: Arc<BTreeMap<String, String>>,
    /// Base URL `owner/repo` resolves against for clones and fetches.
    clone_base: String,
    /// Confine git children (and their hooks) to the git Landlock
    /// tier. `false` in unit tests (spec 15).
    confine_children: bool,
}

impl GitCli {
    pub fn new(
        token: Secret,
        workspace_root: impl Into<PathBuf>,
        direnv_cache: DirenvCache,
        trusted_repos: Vec<String>,
    ) -> Self {
        let warmer = Warmer::new(direnv_cache.clone());
        Self {
            token,
            workspace_root: workspace_root.into(),
            direnv_cache,
            trusted_repos,
            warmer,
            warm_commands: Arc::default(),
            clone_base: "https://github.com".into(),
            confine_children: false,
        }
    }

    /// Override the clone base URL (`git.clone_base`).
    pub fn with_clone_base(mut self, base: &str) -> Self {
        self.clone_base = base.trim_end_matches('/').to_string();
        self
    }

    /// Run git children under the git Landlock tier (spec 15). Hooks
    /// are repo-controlled code; they get exec grants plus the askpass
    /// helper and the keyring instead of the daemon's full grant.
    pub fn with_confinement(mut self, enabled: bool) -> Self {
        self.confine_children = enabled;
        self
    }

    /// The confinement for one git spawn, when enabled.
    fn confinement(&self) -> Option<Confinement> {
        self.confine_children.then(|| Confinement {
            tier: Tier::Git,
            workspace: self.workspace_root.clone(),
        })
    }

    /// Remote URL for `owner/repo`.
    pub fn repo_url(&self, nwo: &str) -> String {
        format!("{}/{nwo}.git", self.clone_base)
    }

    /// Share warm state and configured commands. The runtime calls
    /// this on every `GitCli` it builds so the tool-side and
    /// channel-side instances see one warm map.
    pub fn with_warm(mut self, warmer: Warmer, commands: Arc<BTreeMap<String, String>>) -> Self {
        self.warmer = warmer;
        self.warm_commands = commands;
        self
    }

    /// Shared warm state, for the readiness wait in `git_commit`.
    pub fn warmer(&self) -> &Warmer {
        &self.warmer
    }

    /// Trust and warm the devShell of a prepared checkout, then kick
    /// off the repo's configured build warm in the background
    /// (spec 03: the warm never blocks the turn that triggered it).
    ///
    /// No-op without an `.envrc` or when origin is not in
    /// `trusted_repos`. Best-effort: a failed warm degrades to
    /// no-devshell, which exec already tolerates.
    pub async fn warm_devshell(&self, dir: &Path) {
        let Ok(Some(command)) = self.provision_devshell(dir).await else {
            return;
        };
        let (warmer, dir, command) = (self.warmer.clone(), dir.to_path_buf(), command);
        tokio::spawn(async move { warmer.warm(&dir, &command).await });
    }

    /// Allow and evaluate the devshell of a trusted checkout. Returns
    /// the repo's warm command when a build warm should follow: the
    /// devshell resolved and a command is configured (the warm command
    /// comes from the devshell, so it cannot run without one). `Err`
    /// carries why no devshell was provisioned — the duty summary must
    /// not report a 15-minute eval timeout as a repo without one.
    async fn provision_devshell(&self, dir: &Path) -> Result<Option<String>, String> {
        if !dir.join(".envrc").exists() {
            return Err("no .envrc".into());
        }
        let Some(nwo) = super::origin_nwo(dir).await else {
            debug!(dir = %dir.display(), "no readable origin; skipping devshell warm");
            return Err("no readable origin".into());
        };
        if !super::url::is_trusted_repo(&nwo, &self.trusted_repos) {
            debug!(dir = %dir.display(), "origin not in trusted_repos; skipping devshell warm");
            return Err("origin not in git.repositories".into());
        }
        direnv::allow(dir).await;
        if let Err(e) = self.direnv_cache.get(dir).await {
            warn!(dir = %dir.display(), error = %e, "devshell warm failed");
            return Err(format!("devshell eval failed: {e}"));
        }
        Ok(warm_command(&self.warm_commands, &nwo).map(str::to_string))
    }

    /// Prepare and warm every repo in `warm_commands` — the spec 24
    /// warm duty. Clones missing checkouts. Sequential and awaited on
    /// purpose: two cold builds would contend for the same cores.
    /// Returns a per-repo summary for the duty history log.
    pub async fn warm_configured_repos(&self) -> String {
        let nwos: Vec<String> = self.warm_commands.keys().cloned().collect();
        if nwos.is_empty() {
            return "no warm commands configured".into();
        }
        let mut lines = Vec::with_capacity(nwos.len());
        for nwo in nwos {
            let status = self.prepare_and_warm(&nwo).await;
            lines.push(format!("{nwo}: {status}"));
        }
        lines.join("; ")
    }

    async fn prepare_and_warm(&self, nwo: &str) -> String {
        let rel = match super::checkout::rel_path("projects", nwo) {
            Ok(rel) => rel,
            Err(e) => return format!("bad repo path: {e}"),
        };
        let url = self.repo_url(nwo);
        let dir = match super::checkout::ensure_cloned(self, &url, &rel).await {
            Ok(ensured) => ensured.into_dir(),
            Err(e) => return format!("clone failed: {e}"),
        };
        match self.provision_devshell(&dir).await {
            Err(reason) => format!("skipped: {reason}"),
            Ok(None) => "skipped: no check command".into(),
            Ok(Some(command)) => match self.warmer.warm(&dir, &command).await {
                crate::tools::warm::WarmOutcome::Failed => "failed".into(),
                crate::tools::warm::WarmOutcome::Ready => "warm".into(),
            },
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
    pub fn prepare_git(&self, args: &[&str], cwd: &Path) -> SubprocessCall {
        let env: Vec<(OsString, OsString)> = crate::tools::safe_env().collect();
        SubprocessCall {
            binary: "git",
            args: args.iter().map(ToString::to_string).collect(),
            cwd: cwd.to_path_buf(),
            env,
            timeout_secs: hook_timeout(args),
            stdin: None,
            confine: self.confinement(),
        }
    }

    /// The SHA of the remote default-branch head, via `git ls-remote`.
    /// Needs no checkout — the duty scheduler's new-commits gate probes
    /// repos the bot may never have cloned.
    pub async fn remote_head(&self, nwo: &str) -> Result<String, ToolError> {
        let url = self.repo_url(nwo);
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
            // Blocked is a designed state, not a fault: review worktrees
            // carry the repo's .envrc but are never `direnv allow`ed.
            Err(direnv::DirenvError::Blocked) => {
                debug!(dir = %call.cwd.display(), "envrc not allowed; running git without devshell");
            }
            Err(ref e) => {
                warn!(dir = %call.cwd.display(), error = %e, "direnv failed, running git without devshell");
            }
        }

        let askpass = if authenticated {
            Some(AskPass::create(&self.token, &self.workspace_root).await?)
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

/// Allowance for git subcommands whose work is unbounded by git itself.
///
/// `commit` and `push` run repository hooks — arbitrary repo-defined
/// work; this project's own `pre-commit` runs `just check`. `clone`
/// and `fetch` move whole repositories through the egress proxy: the
/// 120s subprocess default killed the warm duty's clone of a large
/// repo on a cold cache, leaving a partial `.git`. Matches the
/// allowance `direnv` gets for evaluating a flake, the same kind of
/// wait.
const LONG_TIMEOUT_SECS: u64 = 900;

/// The timeout for `args`, if its subcommand runs hooks or transfers
/// a repository. A property of the subcommand rather than the caller,
/// so no call site has to remember. Everything else is bounded by
/// local IO and keeps the default.
fn hook_timeout(args: &[&str]) -> Option<u64> {
    matches!(
        args.first(),
        Some(&"clone" | &"commit" | &"fetch" | &"push")
    )
    .then_some(LONG_TIMEOUT_SECS)
}

/// The configured warm command for `nwo`, matched case-insensitively
/// like every other repo comparison.
fn warm_command<'a>(commands: &'a BTreeMap<String, String>, nwo: &str) -> Option<&'a str> {
    commands
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(nwo))
        .map(|(_, command)| command.as_str())
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
/// The script lives in a per-call temp directory under
/// `state/askpass/` (mode 0700): never under the shared `/tmp`, which
/// the exec tier grants broadly. The directory is owned by a `TempDir`
/// and removed on drop, so cleanup happens even if the git command
/// fails or the future is cancelled.
struct AskPass {
    /// Path to the executable script inside `_dir`.
    path: PathBuf,
    /// Owns the temp directory. Removed on drop.
    _dir: tempfile::TempDir,
}

impl AskPass {
    async fn create(token: &Secret, workspace_root: &Path) -> Result<Self, ToolError> {
        use crate::workspace::{ASKPASS_DIR, STATE_DIR};
        use std::os::unix::fs::PermissionsExt;

        let parent = workspace_root.join(STATE_DIR).join(ASKPASS_DIR);
        tokio::fs::create_dir_all(&parent).await.map_err(|e| {
            ToolError::ExecutionFailed(format!("create askpass dir {}: {e}", parent.display()))
        })?;
        let dir = tempfile::Builder::new()
            .prefix("askpass-")
            .tempdir_in(&parent)
            .map_err(|e| {
                ToolError::ExecutionFailed(format!("askpass tempdir in {}: {e}", parent.display()))
            })?;

        let path = dir.path().join("askpass");
        let script = format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", token.expose());

        tokio::fs::write(&path, &script).await.map_err(|e| {
            ToolError::ExecutionFailed(format!("write askpass {}: {e}", path.display()))
        })?;

        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .await
            .map_err(|e| {
                ToolError::ExecutionFailed(format!("chmod askpass {}: {e}", path.display()))
            })?;

        Ok(Self { path, _dir: dir })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{GitCli, LONG_TIMEOUT_SECS, hook_timeout, parse_ls_remote_head};
    use crate::secrets::Secret;
    use crate::test_support::{ENV_LOCK, FakeDirenv};
    use crate::tools::DirenvCache;
    use crate::tools::git::test_helpers::stub_git_cli_with_repo;

    /// A workspace with a real git checkout at `projects/o/r` whose
    /// origin parses to `o/r`, and a `GitCli` trusting it.
    fn workspace_with_checkout(warm_command: &str) -> (GitCli, std::path::PathBuf) {
        let workspace = tempfile::tempdir().unwrap();
        let dir = workspace.path().join("projects/o/r");
        std::fs::create_dir_all(&dir).unwrap();
        for args in [
            vec!["init"],
            vec!["remote", "add", "origin", "https://github.com/o/r.git"],
        ] {
            let out = std::process::Command::new("git")
                .args(&args)
                .current_dir(&dir)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed");
        }
        std::fs::write(dir.join(".envrc"), "use flake").unwrap();
        let commands: std::collections::BTreeMap<String, String> =
            [("o/r".to_string(), warm_command.to_string())].into();
        let direnv = DirenvCache::new();
        let git = GitCli::new(
            Secret::test("fake"),
            workspace.path(),
            direnv.clone(),
            vec!["o/r".into()],
        )
        .with_warm(crate::tools::Warmer::new(direnv), Arc::new(commands));
        // Leak the tempdir so the checkout outlives the helper.
        let _ = workspace.keep();
        (git, dir)
    }

    #[tokio::test]
    async fn warm_configured_repos_warms_an_existing_checkout() {
        let _lock = ENV_LOCK.lock().await;
        let _fake = FakeDirenv::install("echo '{}'");
        let (git, dir) = workspace_with_checkout("touch .warmed");

        let summary = git.warm_configured_repos().await;

        assert_eq!(summary, "o/r: warm");
        assert!(dir.join(".warmed").exists(), "warm command must have run");
        assert_eq!(
            git.warmer().ready(&dir).await,
            Some(crate::tools::warm::WarmOutcome::Ready)
        );
    }

    #[tokio::test]
    async fn warm_configured_repos_reports_a_failing_command() {
        let _lock = ENV_LOCK.lock().await;
        let _fake = FakeDirenv::install("echo '{}'");
        let (git, dir) = workspace_with_checkout("exit 1");

        assert_eq!(git.warm_configured_repos().await, "o/r: failed");
        assert_eq!(
            git.warmer().ready(&dir).await,
            Some(crate::tools::warm::WarmOutcome::Failed)
        );
    }

    /// Hooks and repo transfers wait; bounded reads do not.
    #[test]
    fn only_unbounded_subcommands_get_the_long_timeout() {
        for slow in [
            vec!["clone", "url", "dir"],
            vec!["commit", "-m", "x"],
            vec!["fetch", "origin"],
            vec!["push", "origin", "b"],
        ] {
            assert_eq!(hook_timeout(&slow), Some(LONG_TIMEOUT_SECS), "{slow:?}");
        }
        for read in [
            vec!["log", "--oneline"],
            vec!["diff", "--cached"],
            vec!["ls-remote", "url", "HEAD"],
        ] {
            assert_eq!(hook_timeout(&read), None, "{read:?}");
        }
        assert_eq!(hook_timeout(&[]), None);
    }

    #[test]
    fn warm_command_matches_case_insensitively() {
        let commands: std::collections::BTreeMap<String, String> =
            [("Owner/Repo".to_string(), "just check".to_string())].into();
        assert_eq!(
            super::warm_command(&commands, "owner/repo"),
            Some("just check")
        );
        assert_eq!(super::warm_command(&commands, "owner/other"), None);
    }

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
