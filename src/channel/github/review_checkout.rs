//! Review checkout preparation.
//!
//! The review checkout at `reviews/<owner>/<repo>` is a git worktree of
//! the repo's working clone at `projects/<owner>/<repo>`. The object
//! store is shared, so a review costs a fetch of the PR head rather
//! than of everything. Before a review turn is dispatched the worktree
//! is force-detached at the recorded PR head SHA, so leftover state
//! from a previous review can never block the next one, and the
//! working tree keeps its own HEAD.
//!
//! No devShell is provisioned: the reviewer sub-agent has no `exec`,
//! and the root only runs `git`/`gh` from the workspace root, so
//! nothing on the review path can consume one. Cloning here must not
//! provision one either, or the cost just moves up a directory.

use std::path::Path;

use crate::error::ToolError;
use crate::tools::git::GitCli;
use crate::tools::git::checkout;

/// Workspace-relative worktree directory for `owner/repo`.
pub(super) fn checkout_rel_path(nwo: &str) -> Result<String, ToolError> {
    checkout::rel_path("reviews", nwo)
}

/// Clone the repo if needed, fetch the PR head and base, and
/// force-detach a worktree at `head_sha`.
///
/// Returns the workspace-relative worktree path.
pub(super) async fn prepare(
    git: &GitCli,
    nwo: &str,
    pr_number: u32,
    head_sha: &str,
    base: &str,
) -> Result<String, ToolError> {
    let rel = checkout_rel_path(nwo)?;
    let clone_rel = checkout::rel_path("projects", nwo)?;
    let url = git.repo_url(nwo);
    prepare_at(git, &url, &clone_rel, &rel, pr_number, head_sha, base).await?;
    Ok(rel)
}

/// URL-parametrized body of [`prepare`], so tests can use `file://`.
async fn prepare_at(
    git: &GitCli,
    url: &str,
    clone_rel: &str,
    rel: &str,
    pr_number: u32,
    head_sha: &str,
    base: &str,
) -> Result<(), ToolError> {
    if head_sha.len() != 40 || !head_sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ToolError::InvalidArguments(format!(
            "head SHA is not a 40-char hex string: {head_sha}"
        )));
    }
    // Both come from the GitHub API, but neither may look like an option.
    if base.is_empty() || base.starts_with('-') {
        return Err(ToolError::InvalidArguments(format!(
            "invalid base ref: {base}"
        )));
    }

    // Reviewing a repo the bot has never worked on clones it. Fetch
    // into the clone, not the worktree: they share refs, and the
    // worktree may not exist yet.
    let clone = checkout::ensure_cloned(git, url, clone_rel)
        .await?
        .into_dir();
    let pull_ref = format!("pull/{pr_number}/head");
    checkout::run(git, &["fetch", "origin", base, &pull_ref], &clone, true).await?;

    let worktree = git.workspace_root().join(rel);
    ensure_worktree(git, &clone, &worktree, head_sha).await?;
    checkout::run(
        git,
        &["checkout", "--force", "--detach", head_sha],
        &worktree,
        false,
    )
    .await?;
    Ok(())
}

/// Register `worktree` against `clone` unless it already is one.
///
/// `--detach` is load-bearing: git refuses to check the same branch out
/// in two worktrees, and a PR head on the same branch the bot is
/// working on would otherwise collide.
async fn ensure_worktree(
    git: &GitCli,
    clone: &Path,
    worktree: &Path,
    head_sha: &str,
) -> Result<(), ToolError> {
    // A worktree's `.git` is a file pointing at the clone; a standalone
    // clone's is a directory. The latter is a checkout from before this
    // was a worktree — discard it, since a detached review checkout
    // holds nothing that is not in the clone or on GitHub.
    let marker = worktree.join(".git");
    if marker.is_file() {
        return Ok(());
    }
    if marker.is_dir() {
        tokio::fs::remove_dir_all(worktree).await.map_err(|e| {
            ToolError::ExecutionFailed(format!("remove {}: {e}", worktree.display()))
        })?;
    }
    let path = worktree
        .to_str()
        .ok_or_else(|| ToolError::InvalidArguments("worktree path is not UTF-8".into()))?;
    // A deleted worktree directory leaves its registration behind, and
    // `add` refuses a path it still believes is registered.
    checkout::run(git, &["worktree", "prune"], clone, false).await?;
    checkout::run(
        git,
        &["worktree", "add", "--detach", path, head_sha],
        clone,
        false,
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::secrets::Secret;
    use crate::tools::DirenvCache;

    // ── checkout_rel_path ───────────────────────────────────────────
    // Path validation lives in `checkout::rel_path`; this only confirms
    // the review prefix is threaded through.

    #[test]
    fn rel_path_uses_reviews_prefix() {
        assert_eq!(
            checkout_rel_path("owner/repo").unwrap(),
            "reviews/owner/repo"
        );
    }

    // ── prepare_at argument validation ──────────────────────────────

    fn stub_git() -> (GitCli, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let cli = GitCli::new(
            Secret::test("fake"),
            dir.path(),
            DirenvCache::new(),
            Vec::new(),
        );
        (cli, dir)
    }

    #[tokio::test]
    async fn prepare_rejects_non_hex_sha() {
        let (git, _dir) = stub_git();
        let err = prepare_at(
            &git,
            "file:///nowhere",
            "projects/o/r",
            "reviews/o/r",
            1,
            "HEAD",
            "main",
        )
        .await;
        assert!(matches!(err, Err(ToolError::InvalidArguments(_))));
    }

    #[tokio::test]
    async fn prepare_rejects_short_sha() {
        let (git, _dir) = stub_git();
        let err = prepare_at(
            &git,
            "file:///nowhere",
            "projects/o/r",
            "reviews/o/r",
            1,
            "abc123",
            "main",
        )
        .await;
        assert!(matches!(err, Err(ToolError::InvalidArguments(_))));
    }

    #[tokio::test]
    async fn prepare_rejects_option_shaped_base() {
        let (git, _dir) = stub_git();
        let sha = "a".repeat(40);
        let err = prepare_at(
            &git,
            "file:///nowhere",
            "projects/o/r",
            "reviews/o/r",
            1,
            &sha,
            "--upload-pack=x",
        )
        .await;
        assert!(matches!(err, Err(ToolError::InvalidArguments(_))));
    }

    // ── Integration against a local fixture repo ────────────────────

    /// Run git in `dir`, panicking on failure. Identity via -c flags so
    /// the fixture works without global git config.
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

    /// Build an origin repo with a `main` commit and a PR commit
    /// reachable only via `refs/pull/1/head`. Returns the PR head SHA.
    fn fixture_origin(dir: &Path) -> String {
        git_in(dir, &["init", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "base\n").unwrap();
        git_in(dir, &["add", "a.txt"]);
        git_in(dir, &["commit", "-m", "base"]);
        git_in(dir, &["checkout", "-b", "pr-branch"]);
        std::fs::write(dir.join("a.txt"), "pr change\n").unwrap();
        git_in(dir, &["commit", "-am", "pr change"]);
        let sha = git_in(dir, &["rev-parse", "HEAD"]).trim().to_string();
        git_in(dir, &["update-ref", "refs/pull/1/head", &sha]);
        git_in(dir, &["checkout", "main"]);
        sha
    }

    #[tokio::test]
    async fn prepare_clones_and_detaches_at_head_sha() {
        let workspace = tempfile::tempdir().unwrap();
        let origin = tempfile::tempdir().unwrap();
        let sha = fixture_origin(origin.path());
        let git = GitCli::new(
            Secret::test("fake"),
            workspace.path(),
            DirenvCache::new(),
            Vec::new(),
        );
        let url = format!("file://{}", origin.path().display());

        prepare_at(&git, &url, "projects/o/r", "reviews/o/r", 1, &sha, "main")
            .await
            .unwrap();

        let checkout = workspace.path().join("reviews/o/r");
        assert_eq!(git_in(&checkout, &["rev-parse", "HEAD"]).trim(), sha);
        assert_eq!(
            std::fs::read_to_string(checkout.join("a.txt")).unwrap(),
            "pr change\n"
        );
        // Detached: rev-parse --abbrev-ref HEAD prints "HEAD".
        assert_eq!(
            git_in(&checkout, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
            "HEAD"
        );
        // A worktree of the working clone, not a clone of its own: the
        // marker is a gitdir file and the clone carries the objects.
        assert!(checkout.join(".git").is_file());
        let clone = workspace.path().join("projects/o/r");
        assert!(clone.join(".git").is_dir());
        assert_eq!(git_in(&clone, &["rev-parse", "HEAD"]).trim().len(), 40);
    }

    /// The working tree keeps its own HEAD. `--detach` is what allows
    /// the review to sit on a commit the clone also has checked out.
    #[tokio::test]
    async fn prepare_leaves_the_working_clone_head_alone() {
        let workspace = tempfile::tempdir().unwrap();
        let origin = tempfile::tempdir().unwrap();
        let sha = fixture_origin(origin.path());
        let git = GitCli::new(
            Secret::test("fake"),
            workspace.path(),
            DirenvCache::new(),
            Vec::new(),
        );
        let url = format!("file://{}", origin.path().display());
        let clone_rel = "projects/o/r";

        prepare_at(&git, &url, clone_rel, "reviews/o/r", 1, &sha, "main")
            .await
            .unwrap();

        let clone = workspace.path().join(clone_rel);
        assert_eq!(
            git_in(&clone, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
            "main"
        );
        assert_ne!(git_in(&clone, &["rev-parse", "HEAD"]).trim(), sha);
    }

    /// Deployments predating worktrees have a standalone clone at the
    /// review path. It is discarded rather than reported: a detached
    /// review checkout holds nothing the clone and GitHub do not.
    #[tokio::test]
    async fn prepare_replaces_a_pre_worktree_review_clone() {
        let workspace = tempfile::tempdir().unwrap();
        let origin = tempfile::tempdir().unwrap();
        let sha = fixture_origin(origin.path());
        let git = GitCli::new(
            Secret::test("fake"),
            workspace.path(),
            DirenvCache::new(),
            Vec::new(),
        );
        let url = format!("file://{}", origin.path().display());

        // Stand in for the old layout: a full clone at reviews/o/r.
        let stale = workspace.path().join("reviews/o/r");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        git_in(workspace.path(), &["clone", &url, stale.to_str().unwrap()]);
        assert!(stale.join(".git").is_dir());

        prepare_at(&git, &url, "projects/o/r", "reviews/o/r", 1, &sha, "main")
            .await
            .unwrap();

        assert!(stale.join(".git").is_file());
        assert_eq!(git_in(&stale, &["rev-parse", "HEAD"]).trim(), sha);
    }

    #[tokio::test]
    async fn prepare_reuses_clone_and_discards_leftover_state() {
        let workspace = tempfile::tempdir().unwrap();
        let origin = tempfile::tempdir().unwrap();
        let sha = fixture_origin(origin.path());
        let git = GitCli::new(
            Secret::test("fake"),
            workspace.path(),
            DirenvCache::new(),
            Vec::new(),
        );
        let url = format!("file://{}", origin.path().display());

        prepare_at(&git, &url, "projects/o/r", "reviews/o/r", 1, &sha, "main")
            .await
            .unwrap();

        // Simulate a previous review turn leaving the checkout dirty
        // and a new commit landing on the PR.
        let checkout = workspace.path().join("reviews/o/r");
        std::fs::write(checkout.join("a.txt"), "local damage\n").unwrap();
        git_in(origin.path(), &["checkout", "pr-branch"]);
        std::fs::write(origin.path().join("a.txt"), "pr v2\n").unwrap();
        git_in(origin.path(), &["commit", "-am", "pr v2"]);
        let sha2 = git_in(origin.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_string();
        git_in(origin.path(), &["update-ref", "refs/pull/1/head", &sha2]);
        git_in(origin.path(), &["checkout", "main"]);

        prepare_at(&git, &url, "projects/o/r", "reviews/o/r", 1, &sha2, "main")
            .await
            .unwrap();

        assert_eq!(git_in(&checkout, &["rev-parse", "HEAD"]).trim(), sha2);
        assert_eq!(
            std::fs::read_to_string(checkout.join("a.txt")).unwrap(),
            "pr v2\n"
        );
    }
}
