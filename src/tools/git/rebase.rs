//! `git_rebase` tool — rebase the current branch onto the moved base.
//!
//! The second sanctioned history rewrite, for the case `git_fixup`
//! refuses by design: the base branch moved and the feature branch
//! must be replayed onto it. No tree invariant can exist here — a
//! conflict resolution changes content by definition — so the
//! mechanical bounds are different, and there are two. The force push
//! is pinned with `--force-with-lease` to the remote branch position
//! observed at `start`'s fetch, so a concurrent push makes the lease
//! fail instead of being overwritten. And `start` requires the local
//! branch to equal its remote-tracking ref, so everything the push
//! publishes is provably this tool's own replay plus in-window
//! conflict resolutions — a locally pre-cooked history (a squash via
//! reset + re-commit) is refused rather than laundered through the
//! lease, which guards only the remote side. Resolution *correctness*
//! has no in-tool guard; that is the PR review's job, and the
//! rewritten commits land in a diff a reviewer reads.
//!
//! Conflict resolution is a conversation, so the tool is a small
//! state machine over the checkout's own rebase state: `start`
//! fetches and begins, a conflict pauses with instructions, the model
//! edits and stages resolutions, `continue` resumes, `abort` restores.
//! Only the terminal success pushes.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;

use super::git_cli::GitCli;
use super::{Tool, ToolCtx};
use crate::error::ToolError;
use crate::tools::cli_runner::CmdOutput;

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum Action {
    /// Give up: restore the branch to its pre-rebase state.
    Abort,
    /// Resume after resolving conflicts (`git add` the resolved files
    /// first, via exec).
    Continue,
    /// Fetch origin and begin rebasing onto the default branch.
    Start,
}

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository directory relative to workspace root
    /// (e.g. `"projects/owner/repo"`).
    repo_dir: String,
    /// `start` a rebase onto the default branch, `continue` after
    /// resolving conflicts, or `abort` to restore the branch.
    action: Action,
}

pub struct Rebase(pub GitCli);

impl Tool for Rebase {
    fn name(&self) -> &'static str {
        "git_rebase"
    }

    fn description(&self) -> &'static str {
        "Rebase the current branch onto the updated default branch and \
         force-push the result (lease-protected). On conflict the \
         rebase pauses: edit the conflicted files, `git add` them via \
         exec, then call again with action \"continue\", or \"abort\" \
         to restore the branch."
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
            let cwd = self.0.resolve_repo_dir(&args.repo_dir)?;
            match args.action {
                Action::Abort => self.abort(&cwd).await,
                Action::Continue => self.resume(&cwd).await,
                Action::Start => self.start(&cwd).await,
            }
        })
    }
}

/// A failed step's message, carrying the git output.
fn step_err(what: &str, out: &CmdOutput) -> ToolError {
    ToolError::CommandFailed {
        command: what.to_string(),
        exit_code: out.exit_code,
        output: format!("{what}: {}\n{}", out.stdout.trim(), out.stderr.trim()),
    }
}

impl Rebase {
    async fn git(&self, cwd: &Path, args: &[&str]) -> Result<CmdOutput, ToolError> {
        let call = self.0.prepare_git(args, cwd);
        self.0.exec_git(call, false).await
    }

    /// Trimmed stdout of a git command that must succeed.
    async fn git_ok(&self, cwd: &Path, args: &[&str], what: &str) -> Result<String, ToolError> {
        let out = self.git(cwd, args).await?;
        if out.exit_code != 0 {
            return Err(step_err(what, &out));
        }
        Ok(out.stdout.trim().to_string())
    }

    /// Whether the checkout has a paused rebase.
    async fn in_progress(&self, cwd: &Path) -> Result<bool, ToolError> {
        // Both backends: rebase-merge (the default) and rebase-apply.
        for dir in ["rebase-merge", "rebase-apply"] {
            let path = self
                .git_ok(cwd, &["rev-parse", "--git-path", dir], "resolve git dir")
                .await?;
            if cwd.join(path).exists() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn start(&self, cwd: &Path) -> Result<String, ToolError> {
        if self.in_progress(cwd).await? {
            return Err(ToolError::Precondition(
                "a rebase is already in progress; continue or abort it first".into(),
            ));
        }

        let branch = self
            .git_ok(
                cwd,
                &["rev-parse", "--abbrev-ref", "HEAD"],
                "resolve branch",
            )
            .await?;
        if branch == "HEAD" {
            return Err(ToolError::Precondition(
                "detached HEAD; check out the branch first".into(),
            ));
        }

        // A dirty tree wedges the rebase midway; refuse up front.
        let unstaged = self.git(cwd, &["diff", "--quiet"]).await?;
        let staged = self.git(cwd, &["diff", "--cached", "--quiet"]).await?;
        if unstaged.exit_code != 0 || staged.exit_code != 0 {
            return Err(ToolError::Precondition(
                "worktree has uncommitted changes; commit or stash them first".into(),
            ));
        }

        let fetch_call = self.0.prepare_git(&["fetch", "origin"], cwd);
        let fetch = self.0.exec_git(fetch_call, true).await?;
        if fetch.exit_code != 0 {
            return Err(step_err("git fetch origin", &fetch));
        }

        let base = self
            .git_ok(
                cwd,
                &["rev-parse", "--abbrev-ref", "origin/HEAD"],
                "resolve default branch",
            )
            .await?;
        if format!("origin/{branch}") == base {
            return Err(ToolError::Precondition(format!(
                "{branch} is the default branch; rebase is for feature branches",
            )));
        }

        // The push at the end is only sound if everything it publishes
        // is this tool's own replay plus visible conflict resolutions.
        // A branch that already diverged from its remote could carry
        // arbitrary pre-cooked history (e.g. a local squash), and the
        // lease cannot see that — it guards the remote side only.
        let local = self
            .git_ok(cwd, &["rev-parse", "HEAD"], "resolve HEAD")
            .await?;
        let remote = self
            .git(cwd, &["rev-parse", "--verify", &format!("origin/{branch}")])
            .await?;
        if remote.exit_code == 0 && remote.stdout.trim() != local {
            return Err(ToolError::Precondition(format!(
                "{branch} has diverged from origin/{branch}; push or restore \
                 it first — this tool only replays what the remote already has",
            )));
        }

        let rebase = self.git(cwd, &["rebase", &base]).await?;
        if rebase.exit_code != 0 {
            return self.pause_or_fail(cwd, &branch, &base, &rebase).await;
        }
        self.push(cwd, &branch, &base).await
    }

    async fn resume(&self, cwd: &Path) -> Result<String, ToolError> {
        if !self.in_progress(cwd).await? {
            return Err(ToolError::Precondition(
                "no rebase in progress; use action \"start\"".into(),
            ));
        }
        // Mid-rebase HEAD is detached; the branch being rebased lives
        // in the rebase state's head-name.
        let Some(branch) = self.rebased_branch(cwd).await? else {
            return Err(ToolError::Precondition(
                "rebase state has no recorded branch; abort and start over".into(),
            ));
        };
        let base = self
            .git_ok(
                cwd,
                &["rev-parse", "--abbrev-ref", "origin/HEAD"],
                "resolve default branch",
            )
            .await?;

        // core.editor=true: --continue reuses the original message;
        // the override only guards the paths where git would prompt.
        let cont = self
            .git(cwd, &["-c", "core.editor=true", "rebase", "--continue"])
            .await?;
        if cont.exit_code != 0 {
            return self.pause_or_fail(cwd, &branch, &base, &cont).await;
        }
        self.push(cwd, &branch, &base).await
    }

    async fn abort(&self, cwd: &Path) -> Result<String, ToolError> {
        if !self.in_progress(cwd).await? {
            return Err(ToolError::Precondition(
                "no rebase in progress; nothing to abort".into(),
            ));
        }
        let out = self.git(cwd, &["rebase", "--abort"]).await?;
        if out.exit_code != 0 {
            return Err(step_err("git rebase --abort", &out));
        }
        Ok("Rebase aborted; the branch is back to its pre-rebase state.".to_string())
    }

    /// The branch name recorded in the paused rebase's state, if any.
    async fn rebased_branch(&self, cwd: &Path) -> Result<Option<String>, ToolError> {
        for dir in ["rebase-merge", "rebase-apply"] {
            let path = self
                .git_ok(cwd, &["rev-parse", "--git-path", dir], "resolve git dir")
                .await?;
            let head_name = cwd.join(path).join("head-name");
            if let Ok(refname) = std::fs::read_to_string(&head_name) {
                return Ok(Some(
                    refname.trim().trim_start_matches("refs/heads/").to_string(),
                ));
            }
        }
        Ok(None)
    }

    /// A stopped rebase is either a conflict (paused, resolvable) or a
    /// genuine failure (not paused).
    async fn pause_or_fail(
        &self,
        cwd: &Path,
        branch: &str,
        base: &str,
        out: &CmdOutput,
    ) -> Result<String, ToolError> {
        if !self.in_progress(cwd).await? {
            return Err(step_err("git rebase", out));
        }
        let conflicted = self
            .git_ok(
                cwd,
                &["diff", "--name-only", "--diff-filter=U"],
                "list conflicts",
            )
            .await?;
        Ok(format!(
            "Rebase of {branch} onto {base} paused on conflicts:\n{conflicted}\n\n\
             Resolve each file (edit away the <<<<<<< markers), stage it \
             with `git add <file>` via exec, then call git_rebase with \
             action \"continue\". Call with action \"abort\" to restore \
             the branch instead.",
        ))
    }

    /// Lease-pinned force push: the expected remote position is the
    /// remote-tracking ref from `start`'s fetch, so a branch that
    /// moved since then fails the lease instead of losing commits.
    async fn push(&self, cwd: &Path, branch: &str, base: &str) -> Result<String, ToolError> {
        let lease = self
            .git(cwd, &["rev-parse", "--verify", &format!("origin/{branch}")])
            .await?;
        let push_args: Vec<String> = if lease.exit_code == 0 {
            let expected = lease.stdout.trim();
            vec![
                "push".into(),
                format!("--force-with-lease={branch}:{expected}"),
                "origin".into(),
                branch.into(),
            ]
        } else {
            // Never pushed: an ordinary publish, nothing to protect.
            vec!["push".into(), "-u".into(), "origin".into(), branch.into()]
        };
        let args: Vec<&str> = push_args.iter().map(String::as_str).collect();
        let push_call = self.0.prepare_git(&args, cwd);
        let push = self.0.exec_git(push_call, true).await?;
        if push.exit_code != 0 {
            return Err(ToolError::CommandFailed {
                command: format!("git push --force-with-lease origin {branch}"),
                exit_code: push.exit_code,
                output: format!(
                    "rebased locally but the push was refused — the remote \
                     branch moved since the fetch. The local rebase stands; \
                     inspect the remote before retrying.\n{}",
                    push.stderr.trim(),
                ),
            });
        }
        Ok(format!(
            "Rebased {branch} onto {base} and force-pushed \
             (lease-protected). History rewritten: the branch's commits \
             have new SHAs.",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::Secret;
    use crate::tools::DirenvCache;

    /// Run git in `dir`, panicking on failure. Identity via -c flags
    /// so the fixture works without global git config.
    fn git_in(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(["-c", "user.email=t@example.com", "-c", "user.name=t"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// A workspace clone of a bare origin: branch `feat` with one
    /// commit, and `main` moved one commit past the branch point.
    /// `conflicting` controls whether main's move touches feat's file.
    fn fixture(conflicting: bool) -> (tempfile::TempDir, Rebase, std::path::PathBuf) {
        let origin = tempfile::tempdir().unwrap();
        git_in(origin.path(), &["init", "--bare", "-b", "main"]);

        let workspace = tempfile::tempdir().unwrap();
        let projects = workspace.path().join("projects");
        std::fs::create_dir_all(&projects).unwrap();
        let url = format!("file://{}", origin.path().display());
        git_in(&projects, &["clone", &url, "r"]);
        let repo = projects.join("r");
        git_in(&repo, &["config", "user.email", "t@example.com"]);
        git_in(&repo, &["config", "user.name", "t"]);

        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        git_in(&repo, &["add", "base.txt"]);
        git_in(&repo, &["commit", "-m", "base"]);
        git_in(&repo, &["push", "origin", "main"]);
        git_in(&repo, &["remote", "set-head", "origin", "main"]);

        git_in(&repo, &["switch", "-c", "feat"]);
        std::fs::write(repo.join("base.txt"), "base\nfeat side\n").unwrap();
        git_in(&repo, &["commit", "-am", "feat work"]);
        git_in(&repo, &["push", "-u", "origin", "feat"]);

        // Move main on the remote from a second clone, like a merged
        // PR would.
        let other = tempfile::tempdir().unwrap();
        git_in(other.path(), &["clone", &url, "o"]);
        let other_repo = other.path().join("o");
        if conflicting {
            std::fs::write(other_repo.join("base.txt"), "base\nmain side\n").unwrap();
        } else {
            std::fs::write(other_repo.join("new.txt"), "unrelated\n").unwrap();
            git_in(&other_repo, &["add", "new.txt"]);
        }
        git_in(&other_repo, &["commit", "-am", "main moved"]);
        git_in(&other_repo, &["push", "origin", "main"]);

        // Origin leaked deliberately: TempDir drop would break pushes.
        std::mem::forget(origin);
        let git = GitCli::new(
            Secret::test("fake"),
            workspace.path(),
            DirenvCache::new(),
            Vec::new(),
        );
        (workspace, Rebase(git), repo)
    }

    #[tokio::test]
    async fn clean_rebase_pushes_the_replayed_branch() {
        let (_ws, tool, repo) = fixture(false);

        let out = tool.start(&repo).await.unwrap();

        assert!(out.contains("force-pushed"), "{out}");
        // The branch now descends from moved main and the remote agrees.
        let log = git_in(&repo, &["log", "--format=%s", "-3"]);
        assert_eq!(
            log.lines().collect::<Vec<_>>(),
            ["feat work", "main moved", "base"]
        );
        let local = git_in(&repo, &["rev-parse", "HEAD"]);
        let remote = git_in(&repo, &["ls-remote", "origin", "feat"]);
        assert!(remote.starts_with(local.trim()));
    }

    #[tokio::test]
    async fn conflict_pauses_with_instructions_then_continue_pushes() {
        let (_ws, tool, repo) = fixture(true);

        let out = tool.start(&repo).await.unwrap();
        assert!(out.contains("paused on conflicts"), "{out}");
        assert!(out.contains("base.txt"), "{out}");

        std::fs::write(repo.join("base.txt"), "base\nmain side\nfeat side\n").unwrap();
        git_in(&repo, &["add", "base.txt"]);

        let done = tool.resume(&repo).await.unwrap();
        assert!(done.contains("force-pushed"), "{done}");
        let local = git_in(&repo, &["rev-parse", "HEAD"]);
        let remote = git_in(&repo, &["ls-remote", "origin", "feat"]);
        assert!(remote.starts_with(local.trim()));
        let shown = git_in(&repo, &["show", "HEAD:base.txt"]);
        assert_eq!(shown, "base\nmain side\nfeat side\n");
    }

    #[tokio::test]
    async fn abort_restores_the_branch() {
        let (_ws, tool, repo) = fixture(true);
        let head = git_in(&repo, &["rev-parse", "HEAD"]);

        tool.start(&repo).await.unwrap();
        let out = tool.abort(&repo).await.unwrap();

        assert!(out.contains("aborted"), "{out}");
        assert_eq!(git_in(&repo, &["rev-parse", "HEAD"]), head);
        assert_eq!(
            git_in(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
            "feat"
        );
    }

    /// The lease is the whole safety story: a concurrent push to the
    /// branch during conflict resolution must fail the push, not be
    /// overwritten.
    #[tokio::test]
    async fn concurrent_branch_push_fails_the_lease() {
        let (_ws, tool, repo) = fixture(true);
        let url = git_in(&repo, &["remote", "get-url", "origin"]);

        tool.start(&repo).await.unwrap();

        // Someone pushes to feat while the rebase sits paused.
        let other = tempfile::tempdir().unwrap();
        git_in(
            other.path(),
            &["clone", "--branch", "feat", url.trim(), "o"],
        );
        let other_repo = other.path().join("o");
        std::fs::write(other_repo.join("theirs.txt"), "concurrent\n").unwrap();
        git_in(&other_repo, &["add", "theirs.txt"]);
        git_in(&other_repo, &["commit", "-m", "concurrent work"]);
        git_in(&other_repo, &["push", "origin", "feat"]);

        std::fs::write(repo.join("base.txt"), "base\nmain side\nfeat side\n").unwrap();
        git_in(&repo, &["add", "base.txt"]);

        let err = tool.resume(&repo).await.unwrap_err();
        assert!(err.to_string().contains("push was refused"), "{err}");
        // The concurrent commit survived on the remote.
        let remote = git_in(&repo, &["ls-remote", "origin", "feat"]);
        let theirs = git_in(&other_repo, &["rev-parse", "HEAD"]);
        assert!(remote.starts_with(theirs.trim()), "{remote} vs {theirs}");
    }

    /// The squash loophole: reset to base, re-commit as one, then use
    /// this tool as the force push. The divergence check closes it.
    #[tokio::test]
    async fn refuses_a_locally_rewritten_branch() {
        let (_ws, tool, repo) = fixture(false);
        git_in(&repo, &["reset", "--soft", "origin/main"]);
        git_in(&repo, &["commit", "-m", "squashed"]);

        let err = tool.start(&repo).await.unwrap_err();

        assert!(err.to_string().contains("diverged"), "{err}");
        // Nothing was pushed: the remote still has the original commit.
        let remote = git_in(&repo, &["ls-remote", "origin", "feat"]);
        let original = git_in(&repo, &["rev-parse", "origin/feat"]);
        assert!(remote.starts_with(original.trim()));
    }

    #[tokio::test]
    async fn refuses_the_default_branch() {
        let (_ws, tool, repo) = fixture(false);
        git_in(&repo, &["switch", "main"]);

        let err = tool.start(&repo).await.unwrap_err();
        assert!(err.to_string().contains("default branch"), "{err}");
    }

    #[tokio::test]
    async fn refuses_a_dirty_worktree() {
        let (_ws, tool, repo) = fixture(false);
        std::fs::write(repo.join("base.txt"), "dirty\n").unwrap();

        let err = tool.start(&repo).await.unwrap_err();
        assert!(err.to_string().contains("uncommitted"), "{err}");
    }

    #[tokio::test]
    async fn continue_without_a_rebase_is_refused() {
        let (_ws, tool, repo) = fixture(false);
        let err = tool.resume(&repo).await.unwrap_err();
        assert!(err.to_string().contains("no rebase in progress"), "{err}");
    }
}
