//! `git_fixup` tool — meld staged changes into an earlier commit.
//!
//! The sanctioned history rewrite: same-base autosquash only, which
//! carries a mechanical safety net — melding a fixup into its target
//! never changes the final tree, so the tool verifies tree identity
//! before and after and rolls everything back on any mismatch or
//! conflict. The force push lives inside this flow and nowhere else;
//! `git_push` stays fast-forward only. Rebasing onto a moved base has
//! no such invariant and stays out of scope: redo the PR instead.

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
struct Args {
    /// Repository directory relative to workspace root
    /// (e.g. `"projects/myrepo"`).
    repo_dir: String,
    /// The commit to meld the staged changes into (SHA, short form
    /// fine). Must be on the current branch and not on the base
    /// branch.
    target: String,
}

pub struct Fixup(pub GitCli);

impl Tool for Fixup {
    fn name(&self) -> &'static str {
        "git_fixup"
    }

    fn description(&self) -> &'static str {
        "Meld the staged changes into an earlier commit of the current \
         branch and force-push the rewritten history. Small tweaks \
         only; on conflict everything is restored and the tweak stays \
         staged for a normal commit."
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
            self.run(&args.repo_dir, &args.target).await
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

impl Fixup {
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

    /// Precondition checks. Returns the current branch and the
    /// target's resolved `(sha, subject)`.
    async fn preflight(
        &self,
        cwd: &Path,
        target: &str,
    ) -> Result<(String, String, String), ToolError> {
        // The tweak must be exactly the staged changes: a dirty
        // worktree would block the rebase halfway through instead.
        let staged = self.git(cwd, &["diff", "--cached", "--quiet"]).await?;
        if staged.exit_code == 0 {
            return Err(ToolError::InvalidArguments(
                "nothing staged; stage the tweak first".into(),
            ));
        }
        let unstaged = self.git(cwd, &["diff", "--quiet"]).await?;
        if unstaged.exit_code != 0 {
            return Err(ToolError::InvalidArguments(
                "worktree has unstaged changes; stage them too or stash them first".into(),
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
            return Err(ToolError::InvalidArguments(
                "detached HEAD; check out the branch first".into(),
            ));
        }

        let line = self
            .git_ok(
                cwd,
                &["log", "-1", "--format=%H %s", target],
                "resolve target commit",
            )
            .await?;
        let (target_sha, subject) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        let (target_sha, subject) = (target_sha.to_string(), subject.to_string());
        let short = &target_sha[..target_sha.len().min(12)];

        let on_branch = self
            .git(cwd, &["merge-base", "--is-ancestor", &target_sha, "HEAD"])
            .await?;
        if on_branch.exit_code != 0 {
            return Err(ToolError::InvalidArguments(format!(
                "{short} is not an ancestor of HEAD",
            )));
        }
        // A commit reachable from the remote default branch is
        // published base history; rewriting it rewrites master.
        let on_base = self
            .git(
                cwd,
                &["merge-base", "--is-ancestor", &target_sha, "origin/HEAD"],
            )
            .await?;
        if on_base.exit_code == 0 {
            return Err(ToolError::InvalidArguments(format!(
                "{short} is on the base branch; only unmerged branch commits can be rewritten",
            )));
        }
        Ok((branch, target_sha, subject))
    }

    async fn run(&self, repo_dir: &str, target: &str) -> Result<String, ToolError> {
        let cwd = self.0.resolve_repo_dir(repo_dir)?;
        let (branch, target_sha, subject) = self.preflight(&cwd, target).await?;
        let short = &target_sha[..target_sha.len().min(12)];

        // Signed like any commit; pre-commit hooks validate the tweak.
        let commit = self.git(&cwd, &["commit", "--fixup", &target_sha]).await?;
        if commit.exit_code != 0 {
            return Err(step_err("fixup commit failed", &commit));
        }
        let fixup_head = self
            .git_ok(&cwd, &["rev-parse", "HEAD"], "resolve HEAD")
            .await?;
        let tree_before = self
            .git_ok(&cwd, &["rev-parse", "HEAD^{tree}"], "resolve tree")
            .await?;

        let rebase = self
            .git(&cwd, &["rebase", "--autosquash", &format!("{target_sha}^")])
            .await?;
        if rebase.exit_code != 0 {
            self.restore(&cwd, &fixup_head).await;
            return Err(ToolError::CommandFailed {
                command: "git rebase --autosquash".to_string(),
                exit_code: rebase.exit_code,
                output: format!(
                    "autosquash hit conflicts; everything is restored and the \
                     tweak is still staged — commit it normally with git_commit \
                     instead.\n{}\n{}",
                    rebase.stdout.trim(),
                    rebase.stderr.trim(),
                ),
            });
        }

        // Melding never changes the final tree; anything else means
        // the rewrite corrupted content and must not survive.
        let tree_after = self
            .git_ok(&cwd, &["rev-parse", "HEAD^{tree}"], "resolve tree")
            .await?;
        if tree_after != tree_before {
            self.restore(&cwd, &fixup_head).await;
            return Err(ToolError::Precondition(
                "tree changed across the autosquash (invariant violation); \
                 everything is restored and the tweak is still staged"
                    .into(),
            ));
        }

        let push_call = self
            .0
            .prepare_git(&["push", "--force-with-lease", "origin", &branch], &cwd);
        let push = self.0.exec_git(push_call, true).await?;
        if push.exit_code != 0 {
            return Err(ToolError::CommandFailed {
                command: format!("git push --force-with-lease origin {branch}"),
                exit_code: push.exit_code,
                output: format!(
                    "history rewritten locally but the push was rejected — the \
                     remote likely moved. Redo the PR from a fresh branch \
                     instead of retrying.\n{}",
                    push.stderr.trim(),
                ),
            });
        }

        Ok(format!(
            "Melded the staged tweak into {short} (\"{subject}\") and \
             force-pushed {branch}. History rewritten: commits after \
             {short} have new SHAs.",
        ))
    }

    /// Roll back to the pre-call state: abort any in-progress rebase,
    /// return to the fixup commit (the rebase's starting point), and
    /// drop it while keeping its changes staged. Best-effort by
    /// design — it runs on paths that are already failing.
    async fn restore(&self, cwd: &Path, fixup_head: &str) {
        let _ = self.git(cwd, &["rebase", "--abort"]).await;
        let _ = self.git(cwd, &["reset", "--hard", fixup_head]).await;
        let _ = self.git(cwd, &["reset", "--soft", "HEAD^"]).await;
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

    /// A workspace clone of a bare origin, on branch `feat` with two
    /// commits past the base. Returns (workspace, tool, repo dir,
    /// first feat commit sha).
    fn fixture() -> (tempfile::TempDir, Fixup, std::path::PathBuf, String) {
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
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        git_in(&repo, &["add", "a.txt"]);
        git_in(&repo, &["commit", "-m", "add a"]);
        let c1 = git_in(&repo, &["rev-parse", "HEAD"]).trim().to_string();
        std::fs::write(repo.join("b.txt"), "two\n").unwrap();
        git_in(&repo, &["add", "b.txt"]);
        git_in(&repo, &["commit", "-m", "add b"]);
        git_in(&repo, &["push", "-u", "origin", "feat"]);

        // Origin leaked deliberately: TempDir drop would break pushes.
        std::mem::forget(origin);
        let git = GitCli::new(
            Secret::test("fake"),
            workspace.path(),
            DirenvCache::new(),
            Vec::new(),
        );
        (workspace, Fixup(git), repo, c1)
    }

    #[tokio::test]
    async fn melds_the_staged_tweak_into_the_target() {
        let (_ws, tool, repo, c1) = fixture();
        std::fs::write(repo.join("a.txt"), "one\ntweak\n").unwrap();
        git_in(&repo, &["add", "a.txt"]);

        let out = tool.run("projects/r", &c1).await.unwrap();

        assert!(out.contains("force-pushed feat"), "{out}");
        // Two commits past base, no fixup! commit, tweak inside "add a".
        let log = git_in(&repo, &["log", "--format=%s", "origin/main..HEAD"]);
        assert_eq!(log.lines().collect::<Vec<_>>(), ["add b", "add a"]);
        let shown = git_in(&repo, &["show", "HEAD^:a.txt"]);
        assert_eq!(shown, "one\ntweak\n");
        // The remote followed the rewrite.
        let local = git_in(&repo, &["rev-parse", "HEAD"]);
        let remote = git_in(&repo, &["ls-remote", "origin", "feat"]);
        assert!(remote.starts_with(local.trim()));
    }

    #[tokio::test]
    async fn conflict_restores_everything_and_keeps_the_tweak_staged() {
        let (_ws, tool, repo, c1) = fixture();
        // "add b" also rewrote a.txt, so melding a conflicting tweak
        // into "add a" cannot replay cleanly.
        std::fs::write(repo.join("a.txt"), "two\n").unwrap();
        git_in(&repo, &["commit", "-am", "rewrite a"]);
        let head = git_in(&repo, &["rev-parse", "HEAD"]);
        std::fs::write(repo.join("a.txt"), "three\n").unwrap();
        git_in(&repo, &["add", "a.txt"]);

        let err = tool.run("projects/r", &c1).await.unwrap_err();

        assert!(err.to_string().contains("still staged"), "{err}");
        assert_eq!(git_in(&repo, &["rev-parse", "HEAD"]), head);
        let staged = git_in(&repo, &["diff", "--cached", "--name-only"]);
        assert_eq!(staged.trim(), "a.txt");
        let log = git_in(&repo, &["log", "--format=%s", "-1"]);
        assert!(
            !log.contains("fixup!"),
            "fixup commit must not survive: {log}"
        );
    }

    #[tokio::test]
    async fn refuses_base_branch_commits() {
        let (_ws, tool, repo, _c1) = fixture();
        let base = git_in(&repo, &["rev-parse", "origin/main"]);
        std::fs::write(repo.join("a.txt"), "one\ntweak\n").unwrap();
        git_in(&repo, &["add", "a.txt"]);

        let err = tool.run("projects/r", base.trim()).await.unwrap_err();

        assert!(err.to_string().contains("base branch"), "{err}");
    }

    #[tokio::test]
    async fn refuses_when_nothing_is_staged() {
        let (_ws, tool, _repo, c1) = fixture();
        let err = tool.run("projects/r", &c1).await.unwrap_err();
        assert!(err.to_string().contains("nothing staged"), "{err}");
    }

    #[tokio::test]
    async fn refuses_unstaged_changes() {
        let (_ws, tool, repo, c1) = fixture();
        std::fs::write(repo.join("a.txt"), "one\ntweak\n").unwrap();
        git_in(&repo, &["add", "a.txt"]);
        std::fs::write(repo.join("b.txt"), "dirty\n").unwrap();

        let err = tool.run("projects/r", &c1).await.unwrap_err();

        assert!(err.to_string().contains("unstaged"), "{err}");
    }
}
