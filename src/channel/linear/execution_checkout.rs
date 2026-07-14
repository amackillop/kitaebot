//! Execution checkout preparation.
//!
//! A Linear execution turn branches off the target repo, so it must
//! start from an up-to-date base. Each repo gets a working checkout
//! under `projects/<owner>/<repo>`; before an execution turn is
//! dispatched, the checkout is fetched and force-detached at the
//! remote's default branch, then cleaned, so a stale clone from an
//! earlier turn can never seed the branch with an outdated base.

use crate::error::ToolError;
use crate::tools::git::GitCli;
use crate::tools::git::checkout;

/// Workspace-relative checkout directory for `owner/repo`.
pub(super) fn checkout_rel_path(nwo: &str) -> Result<String, ToolError> {
    checkout::rel_path("projects", nwo)
}

/// Clone the repo if needed, fetch, force-detach at the remote's default
/// branch, and drop any leftover files.
///
/// Returns the workspace-relative checkout path.
pub(super) async fn prepare(git: &GitCli, nwo: &str) -> Result<String, ToolError> {
    let rel = checkout_rel_path(nwo)?;
    let url = format!("https://github.com/{nwo}.git");
    prepare_at(git, &url, &rel).await?;
    Ok(rel)
}

/// In-repo caches the per-turn clean must not touch: re-provisioning
/// them costs far more than tolerating a stale entry.
const KEPT_CACHES: &[&str] = &[".direnv", "node_modules", "target", ".venv"];

/// URL-parametrized body of [`prepare`], so tests can use `file://`.
async fn prepare_at(git: &GitCli, url: &str, rel: &str) -> Result<(), ToolError> {
    let dir = checkout::ensure_cloned(git, url, rel).await?;
    checkout::run(git, &["fetch", "origin"], &dir, true).await?;
    checkout::run(
        git,
        &["checkout", "--force", "--detach", "origin/HEAD"],
        &dir,
        false,
    )
    .await?;
    // Sweep untracked and ignored leftovers, but keep the caches.
    let mut clean = vec!["clean", "-fdx"];
    for kept in KEPT_CACHES {
        clean.extend(["-e", kept]);
    }
    checkout::run(git, &clean, &dir, false).await?;
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
    // the projects/ prefix is threaded through.

    #[test]
    fn rel_path_uses_projects_prefix() {
        assert_eq!(
            checkout_rel_path("owner/repo").unwrap(),
            "projects/owner/repo"
        );
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

    /// Build an origin repo with a `main` default branch. Returns the
    /// current `main` head SHA.
    fn fixture_origin(dir: &Path) -> String {
        git_in(dir, &["init", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "base\n").unwrap();
        git_in(dir, &["add", "a.txt"]);
        git_in(dir, &["commit", "-m", "base"]);
        git_in(dir, &["rev-parse", "HEAD"]).trim().to_string()
    }

    #[tokio::test]
    async fn prepare_clones_and_detaches_at_default_head() {
        let workspace = tempfile::tempdir().unwrap();
        let origin = tempfile::tempdir().unwrap();
        let sha = fixture_origin(origin.path());
        let git = GitCli::new(Secret::test("fake"), workspace.path(), DirenvCache::new());
        let url = format!("file://{}", origin.path().display());

        prepare_at(&git, &url, "projects/o/r").await.unwrap();

        let checkout = workspace.path().join("projects/o/r");
        assert_eq!(git_in(&checkout, &["rev-parse", "HEAD"]).trim(), sha);
        // Detached: rev-parse --abbrev-ref HEAD prints "HEAD".
        assert_eq!(
            git_in(&checkout, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
            "HEAD"
        );
    }

    #[tokio::test]
    async fn prepare_reuses_clone_and_picks_up_new_base_commits() {
        let workspace = tempfile::tempdir().unwrap();
        let origin = tempfile::tempdir().unwrap();
        fixture_origin(origin.path());
        let git = GitCli::new(Secret::test("fake"), workspace.path(), DirenvCache::new());
        let url = format!("file://{}", origin.path().display());

        prepare_at(&git, &url, "projects/o/r").await.unwrap();

        // A previous turn left the checkout dirty (tracked edit plus an
        // untracked file); the base advanced.
        let checkout = workspace.path().join("projects/o/r");
        std::fs::write(checkout.join("a.txt"), "local damage\n").unwrap();
        std::fs::write(checkout.join("leftover.txt"), "junk\n").unwrap();
        std::fs::write(origin.path().join("a.txt"), "base v2\n").unwrap();
        git_in(origin.path(), &["commit", "-am", "base v2"]);
        let sha2 = git_in(origin.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_string();

        prepare_at(&git, &url, "projects/o/r").await.unwrap();

        assert_eq!(git_in(&checkout, &["rev-parse", "HEAD"]).trim(), sha2);
        assert_eq!(
            std::fs::read_to_string(checkout.join("a.txt")).unwrap(),
            "base v2\n"
        );
        assert!(!checkout.join("leftover.txt").exists());
    }

    #[tokio::test]
    async fn prepare_preserves_devshell_caches() {
        let workspace = tempfile::tempdir().unwrap();
        let origin = tempfile::tempdir().unwrap();
        fixture_origin(origin.path());
        let git = GitCli::new(Secret::test("fake"), workspace.path(), DirenvCache::new());
        let url = format!("file://{}", origin.path().display());

        prepare_at(&git, &url, "projects/o/r").await.unwrap();

        // An earlier turn provisioned the devShell caches and left junk.
        let checkout = workspace.path().join("projects/o/r");
        std::fs::create_dir(checkout.join("node_modules")).unwrap();
        std::fs::write(checkout.join("node_modules/dep.js"), "cached\n").unwrap();
        std::fs::create_dir(checkout.join(".direnv")).unwrap();
        std::fs::write(checkout.join(".direnv/env"), "cached\n").unwrap();
        std::fs::create_dir(checkout.join("target")).unwrap();
        std::fs::write(checkout.join("target/lib.rlib"), "cached\n").unwrap();
        std::fs::write(checkout.join("stale.log"), "junk\n").unwrap();

        prepare_at(&git, &url, "projects/o/r").await.unwrap();

        assert!(
            checkout.join("node_modules/dep.js").exists(),
            "node_modules must survive the clean"
        );
        assert!(
            checkout.join(".direnv/env").exists(),
            ".direnv must survive the clean"
        );
        assert!(
            checkout.join("target/lib.rlib").exists(),
            "target must survive the clean"
        );
        assert!(
            !checkout.join("stale.log").exists(),
            "unrelated leftovers are still swept"
        );
    }
}
