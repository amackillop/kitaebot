//! Shared helpers for channel-prepared checkouts.
//!
//! Both the GitHub review path (`reviews/<owner>/<repo>`) and the
//! Linear execution path (`projects/<owner>/<repo>`) clone a repo into
//! a per-repo directory, then position HEAD before a turn runs. The
//! cloning and path plumbing are identical; only the refs each fetches
//! and where it parks HEAD differ.

use std::path::{Path, PathBuf};

use super::GitCli;
use super::url::validate_name;
use crate::error::ToolError;

/// Workspace-relative checkout dir `<root>/<owner>/<repo>`.
pub(crate) fn rel_path(root: &str, nwo: &str) -> Result<String, ToolError> {
    let (owner, repo) = nwo
        .split_once('/')
        .ok_or_else(|| ToolError::InvalidArguments(format!("expected owner/repo, got: {nwo}")))?;
    Ok(format!(
        "{root}/{}/{}",
        validate_name(owner)?,
        validate_name(repo)?
    ))
}

/// How [`ensure_cloned`] satisfied the request. Callers that treat a
/// fresh clone differently from a reused checkout (`git_clone` fetches
/// the latter) branch on this instead of re-deriving it from disk.
pub(crate) enum Ensured {
    /// A clone ran: the directory was missing, or held a corrupt
    /// skeleton that was replaced.
    Cloned(PathBuf),
    /// A healthy checkout already existed and was left untouched.
    Existing(PathBuf),
}

impl Ensured {
    pub(crate) fn into_dir(self) -> PathBuf {
        match self {
            Self::Cloned(dir) | Self::Existing(dir) => dir,
        }
    }
}

/// Clone `url` into the workspace-relative `rel` unless a healthy
/// checkout already exists.
pub(crate) async fn ensure_cloned(
    git: &GitCli,
    url: &str,
    rel: &str,
) -> Result<Ensured, ToolError> {
    let dir = git.workspace_root().join(rel);
    if dir.join(".git").is_dir() {
        // rev-parse is local-only: it fails only when the repo is
        // genuinely broken, never for a checkout holding work.
        if run(git, &["rev-parse", "--git-dir"], &dir, false)
            .await
            .is_ok()
        {
            return Ok(Ensured::Existing(dir));
        }
        // A clone interrupted by machine death leaves a .git skeleton
        // git rejects; trusting is_dir() would wedge this repo forever.
        tokio::fs::remove_dir_all(&dir)
            .await
            .map_err(|e| ToolError::Io {
                operation: "remove",
                path: dir.clone(),
                source: e,
            })?;
    }
    let parent = dir.parent().expect("rel path has parent components");
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|e| ToolError::Io {
            operation: "create",
            path: parent.to_path_buf(),
            source: e,
        })?;
    let name = rel.rsplit('/').next().expect("rsplit yields at least one");
    if let Err(e) = run(git, &["clone", url, name], parent, true).await {
        // A failed clone must not leave a partial .git either.
        let _ = tokio::fs::remove_dir_all(&dir).await;
        return Err(e);
    }
    Ok(Ensured::Cloned(dir))
}

/// Run git in `cwd`, failing on nonzero exit, with optional credential
/// injection.
pub(crate) async fn run(
    git: &GitCli,
    args: &[&str],
    cwd: &Path,
    authenticated: bool,
) -> Result<(), ToolError> {
    let call = git.prepare_git(args, cwd);
    let out = git.exec_git(call, authenticated).await?;
    if out.exit_code != 0 {
        return Err(ToolError::CommandFailed {
            command: format!("git {}", args.join(" ")),
            exit_code: out.exit_code,
            output: format!(
                "git {} exited {}: {}",
                args.first().copied().unwrap_or(""),
                out.exit_code,
                out.stderr.trim(),
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::Secret;
    use crate::tools::DirenvCache;

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

    #[tokio::test]
    async fn run_fails_on_nonzero_exit() {
        let (git, dir) = workspace_git();
        // Not a repo: `git log` exits nonzero, which must be an error,
        // not silently discarded output.
        let err = run(&git, &["log", "-1"], dir.path(), false).await;
        assert!(matches!(err, Err(ToolError::CommandFailed { .. })));
    }

    #[tokio::test]
    async fn ensure_cloned_fails_loudly_and_leaves_no_partial_checkout() {
        let (git, dir) = workspace_git();
        let err = ensure_cloned(&git, "file:///nowhere-at-all", "projects/o/r").await;
        assert!(matches!(err, Err(ToolError::CommandFailed { .. })));
        assert!(
            !dir.path().join("projects/o/r").exists(),
            "a failed clone must not leave a directory that blocks retries"
        );
    }

    #[tokio::test]
    async fn ensure_cloned_replaces_a_corrupt_skeleton() {
        let (git, dir) = workspace_git();
        // Fixture origin to clone from.
        let origin = tempfile::tempdir().unwrap();
        for args in [
            &["init", "-b", "main"][..],
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "--allow-empty",
                "-m",
                "x",
            ],
        ] {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(origin.path())
                .output()
                .unwrap();
            assert!(out.status.success());
        }
        // What a machine death mid-clone leaves behind: a .git dir
        // that exists but git refuses to recognize.
        let wedged = dir.path().join("projects/o/r/.git");
        std::fs::create_dir_all(&wedged).unwrap();

        let url = format!("file://{}", origin.path().display());
        let ensured = ensure_cloned(&git, &url, "projects/o/r").await.unwrap();

        assert!(
            matches!(ensured, Ensured::Cloned(_)),
            "a rejected skeleton must be reported as a fresh clone"
        );
        assert!(
            run(
                &git,
                &["rev-parse", "--git-dir"],
                ensured.into_dir().as_path(),
                false
            )
            .await
            .is_ok(),
            "the skeleton must be replaced by a healthy clone"
        );
    }

    #[tokio::test]
    async fn ensure_cloned_leaves_a_healthy_checkout_alone() {
        let (git, dir) = workspace_git();
        let checkout = dir.path().join("projects/o/r");
        std::fs::create_dir_all(&checkout).unwrap();
        let out = std::process::Command::new("git")
            .args(["init"])
            .current_dir(&checkout)
            .output()
            .unwrap();
        assert!(out.status.success());
        std::fs::write(checkout.join("work.txt"), "uncommitted").unwrap();

        // Unreachable URL: proves no clone is attempted.
        let ensured = ensure_cloned(&git, "file:///nowhere-at-all", "projects/o/r")
            .await
            .unwrap();

        assert!(matches!(ensured, Ensured::Existing(_)));
        assert_eq!(
            std::fs::read_to_string(checkout.join("work.txt")).unwrap(),
            "uncommitted",
            "an existing healthy checkout must never be touched"
        );
    }

    #[test]
    fn rel_path_joins_owner_and_repo_under_root() {
        assert_eq!(
            rel_path("reviews", "owner/repo").unwrap(),
            "reviews/owner/repo"
        );
        assert_eq!(
            rel_path("projects", "owner/repo").unwrap(),
            "projects/owner/repo"
        );
    }

    #[test]
    fn rel_path_rejects_missing_slash() {
        assert!(rel_path("reviews", "just-a-repo").is_err());
    }

    #[test]
    fn rel_path_rejects_extra_segments() {
        assert!(rel_path("reviews", "a/b/c").is_err());
    }

    #[test]
    fn rel_path_rejects_traversal() {
        assert!(rel_path("reviews", "../escape").is_err());
        assert!(rel_path("reviews", "owner/..").is_err());
    }

    #[test]
    fn rel_path_rejects_option_shaped_segments() {
        assert!(rel_path("reviews", "-flag/repo").is_err());
        assert!(rel_path("reviews", "owner/-flag").is_err());
    }
}
