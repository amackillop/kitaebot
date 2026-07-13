//! Review checkout preparation.
//!
//! Each reviewed repo gets its own clone under `reviews/<owner>/<repo>`,
//! separate from the working checkout in `projects/`. Before a review
//! turn is dispatched, the checkout is force-detached at the recorded
//! PR head SHA, so leftover state from a previous review can never
//! block the next one and review turns never touch in-progress work.

use crate::error::ToolError;
use crate::tools::git::GitCli;
use crate::tools::git::checkout;

/// Workspace-relative checkout directory for `owner/repo`.
pub(super) fn checkout_rel_path(nwo: &str) -> Result<String, ToolError> {
    checkout::rel_path("reviews", nwo)
}

/// Clone the repo if needed, fetch the PR head and base, and
/// force-detach the checkout at `head_sha`.
///
/// Returns the workspace-relative checkout path.
pub(super) async fn prepare(
    git: &GitCli,
    nwo: &str,
    pr_number: u32,
    head_sha: &str,
    base: &str,
) -> Result<String, ToolError> {
    let rel = checkout_rel_path(nwo)?;
    let url = format!("https://github.com/{nwo}.git");
    prepare_at(git, &url, &rel, pr_number, head_sha, base).await?;
    Ok(rel)
}

/// URL-parametrized body of [`prepare`], so tests can use `file://`.
async fn prepare_at(
    git: &GitCli,
    url: &str,
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

    let dir = checkout::ensure_cloned(git, url, rel).await?;

    let pull_ref = format!("pull/{pr_number}/head");
    checkout::run(git, &["fetch", "origin", base, &pull_ref], &dir, true).await?;
    checkout::run(
        git,
        &["checkout", "--force", "--detach", head_sha],
        &dir,
        false,
    )
    .await?;
    Ok(())
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
        let cli = GitCli::new(Secret::test("fake"), dir.path(), DirenvCache::new());
        (cli, dir)
    }

    #[tokio::test]
    async fn prepare_rejects_non_hex_sha() {
        let (git, _dir) = stub_git();
        let err = prepare_at(&git, "file:///nowhere", "reviews/o/r", 1, "HEAD", "main").await;
        assert!(matches!(err, Err(ToolError::InvalidArguments(_))));
    }

    #[tokio::test]
    async fn prepare_rejects_short_sha() {
        let (git, _dir) = stub_git();
        let err = prepare_at(&git, "file:///nowhere", "reviews/o/r", 1, "abc123", "main").await;
        assert!(matches!(err, Err(ToolError::InvalidArguments(_))));
    }

    #[tokio::test]
    async fn prepare_rejects_option_shaped_base() {
        let (git, _dir) = stub_git();
        let sha = "a".repeat(40);
        let err = prepare_at(
            &git,
            "file:///nowhere",
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
        let git = GitCli::new(Secret::test("fake"), workspace.path(), DirenvCache::new());
        let url = format!("file://{}", origin.path().display());

        prepare_at(&git, &url, "reviews/o/r", 1, &sha, "main")
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
    }

    #[tokio::test]
    async fn prepare_reuses_clone_and_discards_leftover_state() {
        let workspace = tempfile::tempdir().unwrap();
        let origin = tempfile::tempdir().unwrap();
        let sha = fixture_origin(origin.path());
        let git = GitCli::new(Secret::test("fake"), workspace.path(), DirenvCache::new());
        let url = format!("file://{}", origin.path().display());

        prepare_at(&git, &url, "reviews/o/r", 1, &sha, "main")
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

        prepare_at(&git, &url, "reviews/o/r", 1, &sha2, "main")
            .await
            .unwrap();

        assert_eq!(git_in(&checkout, &["rev-parse", "HEAD"]).trim(), sha2);
        assert_eq!(
            std::fs::read_to_string(checkout.join("a.txt")).unwrap(),
            "pr v2\n"
        );
    }
}
