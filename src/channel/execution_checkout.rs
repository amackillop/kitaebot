//! Execution checkout preparation.
//!
//! An issue-driven execution turn branches off the target repo, so it
//! must start from an up-to-date base. Each repo gets a working
//! checkout under `projects/<owner>/<repo>`; before an execution turn
//! is dispatched, the checkout is fetched and force-detached at the
//! remote's default branch, then cleaned, so a stale clone from an
//! earlier turn can never seed the branch with an outdated base.
//!
//! Preserve, then reset: a predecessor turn that died mid-work leaves
//! a dirty tree or commits stranded on a detached HEAD, and the reset
//! would destroy them. Leftovers are parked on a `kitaebot_recovered/`
//! branch first; the ready note names it so the successor can decide
//! what the work is worth.

use std::path::Path;

use crate::error::ToolError;
use crate::tools::git::GitCli;
use crate::tools::git::checkout;

/// Guidance when no fresh checkout could be prepared for the agent.
pub(crate) const CLONE_YOURSELF: &str = "Clone or update the repo yourself before branching.";

/// A checkout [`prepare`] made ready for an execution turn.
pub(crate) struct Prepared {
    /// Workspace-relative checkout directory.
    pub(crate) rel: String,
    /// Branch holding a predecessor's parked leftovers, when any.
    pub(crate) parked: Option<String>,
}

impl Prepared {
    /// Describe the prepared checkout for the agent.
    pub(crate) fn ready_note(&self) -> String {
        use std::fmt::Write as _;

        let mut note = format!(
            "A fresh checkout at the default branch is ready at {} \
             (use working_dir: \"{}\"). Branch from there; do not clone.",
            self.rel, self.rel,
        );
        if let Some(parked) = &self.parked {
            let _ = write!(
                note,
                " A previous turn left unfinished work, parked on branch \
                 {parked}: inspect it before starting (git log/diff \
                 origin/HEAD..{parked}) and salvage or ignore it."
            );
        }
        note
    }
}

/// Workspace-relative checkout directory for `owner/repo`.
pub(crate) fn checkout_rel_path(nwo: &str) -> Result<String, ToolError> {
    checkout::rel_path("projects", nwo)
}

/// Clone the repo if needed, park a predecessor's leftovers, fetch,
/// force-detach at the remote's default branch, and drop any leftover
/// files.
pub(crate) async fn prepare(git: &GitCli, nwo: &str) -> Result<Prepared, ToolError> {
    let rel = checkout_rel_path(nwo)?;
    let url = git.repo_url(nwo);
    let parked = prepare_at(git, &url, &rel).await?;
    Ok(Prepared { rel, parked })
}

/// In-repo caches the per-turn clean must not touch: re-provisioning
/// them costs far more than tolerating a stale entry.
const KEPT_CACHES: &[&str] = &[".direnv", ".gcroots", "node_modules", ".venv"];

/// Branch prefix for parked leftovers. Epoch-suffixed, so repeated
/// interruptions never collide.
const RECOVERED_PREFIX: &str = "kitaebot_recovered/";

/// URL-parametrized body of [`prepare`], so tests can use `file://`.
async fn prepare_at(git: &GitCli, url: &str, rel: &str) -> Result<Option<String>, ToolError> {
    let (dir, parked) = match checkout::ensure_cloned(git, url, rel).await? {
        // A fresh clone has nothing to lose.
        checkout::Ensured::Cloned(dir) => (dir, None),
        checkout::Ensured::Existing(dir) => {
            let parked = park_leftovers(git, &dir).await?;
            (dir, parked)
        }
    };
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
    // Provision the devShell (trust + dependency install) before the
    // turn starts, so exec finds a working toolchain immediately.
    git.warm_devshell(&dir).await;
    Ok(parked)
}

/// Park a predecessor's leftovers on a branch before the reset
/// destroys them: uncommitted changes are committed there, and
/// commits stranded on a detached HEAD get a name that survives the
/// re-detach. Returns the branch name when anything was parked.
///
/// The kept caches are excluded from the parked commit: committing
/// them would make the later tree-switch delete them from the
/// worktree, defeating the exclusions the clean honors.
async fn park_leftovers(git: &GitCli, dir: &Path) -> Result<Option<String>, ToolError> {
    let mut pathspec = vec![".".to_string()];
    pathspec.extend(KEPT_CACHES.iter().map(|c| format!(":(exclude){c}")));
    let pathspec: Vec<&str> = pathspec.iter().map(String::as_str).collect();

    let mut status = vec!["status", "--porcelain", "--"];
    status.extend(&pathspec);
    let (_, dirty) = checkout::run_read(git, &status, dir).await?;
    let dirty = !dirty.trim().is_empty();
    // Only a detached HEAD strands commits — a branch-attached HEAD
    // (the normal state after a successful turn) keeps its own.
    // symbolic-ref exits non-zero when detached.
    let (attached, _) = checkout::run_read(git, &["symbolic-ref", "-q", "HEAD"], dir).await?;
    let stranded = if attached == 0 {
        false
    } else {
        // Exit 0 = HEAD reachable from origin/HEAD = nothing to lose.
        let (ancestor, _) = checkout::run_read(
            git,
            &["merge-base", "--is-ancestor", "HEAD", "origin/HEAD"],
            dir,
        )
        .await?;
        ancestor != 0
    };
    if !dirty && !stranded {
        return Ok(None);
    }

    let name = format!("{RECOVERED_PREFIX}{}", crate::time::now_epoch());
    checkout::run(git, &["checkout", "-b", &name], dir, false).await?;
    if dirty {
        let mut add = vec!["add", "-A", "--"];
        add.extend(&pathspec);
        checkout::run(git, &add, dir, false).await?;
        // Hermetic identity, unsigned, hook-free: this is a checkpoint
        // of a possibly-broken tree, and a pre-commit hook that gates
        // on that tree would abort the very rescue it needs.
        checkout::run(
            git,
            &[
                "-c",
                "user.name=kitaebot",
                "-c",
                "user.email=kitaebot@localhost",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--no-verify",
                "--allow-empty",
                "-m",
                "Park leftover work from an interrupted turn",
            ],
            dir,
            false,
        )
        .await?;
    }
    Ok(Some(name))
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

    #[test]
    fn ready_note_names_the_parked_branch_only_when_present() {
        let clean = Prepared {
            rel: "projects/o/r".into(),
            parked: None,
        };
        assert!(!clean.ready_note().contains("parked"));
        let parked = Prepared {
            rel: "projects/o/r".into(),
            parked: Some("kitaebot_recovered/123".into()),
        };
        let note = parked.ready_note();
        assert!(note.contains("kitaebot_recovered/123"), "{note}");
        assert!(
            note.contains("origin/HEAD..kitaebot_recovered/123"),
            "{note}"
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
        let git = GitCli::new(
            Secret::test("fake"),
            workspace.path(),
            DirenvCache::new(),
            Vec::new(),
        );
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
        let git = GitCli::new(
            Secret::test("fake"),
            workspace.path(),
            DirenvCache::new(),
            Vec::new(),
        );
        let url = format!("file://{}", origin.path().display());

        assert!(
            prepare_at(&git, &url, "projects/o/r")
                .await
                .unwrap()
                .is_none(),
            "fresh clone parks nothing"
        );

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

        let parked = prepare_at(&git, &url, "projects/o/r")
            .await
            .unwrap()
            .expect("dirty tree must be parked");

        assert_eq!(git_in(&checkout, &["rev-parse", "HEAD"]).trim(), sha2);
        assert_eq!(
            std::fs::read_to_string(checkout.join("a.txt")).unwrap(),
            "base v2\n"
        );
        assert!(!checkout.join("leftover.txt").exists());
        // The parked branch holds both the tracked edit and the
        // untracked file.
        let files = git_in(&checkout, &["show", "--stat", "--format=", &parked]);
        assert!(files.contains("a.txt"), "{files}");
        assert!(files.contains("leftover.txt"), "{files}");
        let content = git_in(&checkout, &["show", &format!("{parked}:a.txt")]);
        assert_eq!(content, "local damage\n");
    }

    #[tokio::test]
    async fn prepare_parks_nothing_on_a_clean_reused_checkout() {
        let workspace = tempfile::tempdir().unwrap();
        let origin = tempfile::tempdir().unwrap();
        fixture_origin(origin.path());
        let git = GitCli::new(
            Secret::test("fake"),
            workspace.path(),
            DirenvCache::new(),
            Vec::new(),
        );
        let url = format!("file://{}", origin.path().display());

        prepare_at(&git, &url, "projects/o/r").await.unwrap();
        assert!(
            prepare_at(&git, &url, "projects/o/r")
                .await
                .unwrap()
                .is_none(),
            "clean reuse parks nothing"
        );
        let checkout = workspace.path().join("projects/o/r");
        let branches = git_in(&checkout, &["branch", "--list", "kitaebot_recovered/*"]);
        assert!(branches.trim().is_empty(), "{branches}");
    }

    #[tokio::test]
    async fn prepare_parks_nothing_after_a_successful_turn_on_a_branch() {
        let workspace = tempfile::tempdir().unwrap();
        let origin = tempfile::tempdir().unwrap();
        fixture_origin(origin.path());
        let git = GitCli::new(
            Secret::test("fake"),
            workspace.path(),
            DirenvCache::new(),
            Vec::new(),
        );
        let url = format!("file://{}", origin.path().display());
        prepare_at(&git, &url, "projects/o/r").await.unwrap();

        // A successful turn's end state: clean tree, HEAD attached to
        // the work branch it pushed, ahead of origin/HEAD. The branch
        // keeps its own commits; nothing is stranded.
        let checkout = workspace.path().join("projects/o/r");
        git_in(&checkout, &["checkout", "-b", "kitaebot_issue-9_fix"]);
        std::fs::write(checkout.join("fix.rs"), "fn fix() {}\n").unwrap();
        git_in(&checkout, &["add", "fix.rs"]);
        git_in(&checkout, &["commit", "-m", "fix"]);

        assert!(
            prepare_at(&git, &url, "projects/o/r")
                .await
                .unwrap()
                .is_none(),
            "a branch-attached HEAD must not be parked"
        );
        let branches = git_in(&checkout, &["branch", "--list", "kitaebot_recovered/*"]);
        assert!(branches.trim().is_empty(), "{branches}");
        // The work branch itself survives untouched.
        let sha = git_in(&checkout, &["rev-parse", "kitaebot_issue-9_fix"]);
        assert!(!sha.trim().is_empty());
    }

    #[tokio::test]
    async fn prepare_parks_commits_stranded_on_a_detached_head() {
        let workspace = tempfile::tempdir().unwrap();
        let origin = tempfile::tempdir().unwrap();
        fixture_origin(origin.path());
        let git = GitCli::new(
            Secret::test("fake"),
            workspace.path(),
            DirenvCache::new(),
            Vec::new(),
        );
        let url = format!("file://{}", origin.path().display());
        prepare_at(&git, &url, "projects/o/r").await.unwrap();

        // A previous turn committed on the detached HEAD and died
        // before pushing or branching; the tree is clean.
        let checkout = workspace.path().join("projects/o/r");
        std::fs::write(checkout.join("wip.rs"), "fn wip() {}\n").unwrap();
        git_in(&checkout, &["add", "wip.rs"]);
        git_in(&checkout, &["commit", "-m", "wip"]);
        let stranded = git_in(&checkout, &["rev-parse", "HEAD"]).trim().to_string();

        let parked = prepare_at(&git, &url, "projects/o/r")
            .await
            .unwrap()
            .expect("stranded commit must be parked");

        // The branch names the stranded commit; HEAD is back on base.
        assert_eq!(git_in(&checkout, &["rev-parse", &parked]).trim(), stranded);
        assert!(!checkout.join("wip.rs").exists());
    }

    #[tokio::test]
    async fn prepare_preserves_devshell_caches() {
        let workspace = tempfile::tempdir().unwrap();
        let origin = tempfile::tempdir().unwrap();
        fixture_origin(origin.path());
        let git = GitCli::new(
            Secret::test("fake"),
            workspace.path(),
            DirenvCache::new(),
            Vec::new(),
        );
        let url = format!("file://{}", origin.path().display());

        prepare_at(&git, &url, "projects/o/r").await.unwrap();

        // An earlier turn provisioned the devShell caches and left junk.
        let checkout = workspace.path().join("projects/o/r");
        std::fs::create_dir(checkout.join("node_modules")).unwrap();
        std::fs::write(checkout.join("node_modules/dep.js"), "cached\n").unwrap();
        std::fs::create_dir(checkout.join(".direnv")).unwrap();
        std::fs::write(checkout.join(".direnv/env"), "cached\n").unwrap();
        std::fs::create_dir(checkout.join(".gcroots")).unwrap();
        std::fs::write(checkout.join(".gcroots/deps"), "root\n").unwrap();
        std::fs::create_dir(checkout.join("target")).unwrap();
        std::fs::write(checkout.join("target/lib.rlib"), "cached\n").unwrap();
        std::fs::write(checkout.join("stale.log"), "junk\n").unwrap();

        let parked = prepare_at(&git, &url, "projects/o/r")
            .await
            .unwrap()
            .expect("stale.log and target are parked as leftovers");
        // The kept caches must stay out of the parked commit: tracked
        // cache files would be deleted from the worktree by the
        // tree-switch back to base.
        let files = git_in(&checkout, &["show", "--stat", "--format=", &parked]);
        assert!(!files.contains("node_modules"), "{files}");
        assert!(!files.contains(".gcroots"), "{files}");

        assert!(
            checkout.join("node_modules/dep.js").exists(),
            "node_modules must survive the clean"
        );
        assert!(
            checkout.join(".direnv/env").exists(),
            ".direnv must survive the clean"
        );
        assert!(
            checkout.join(".gcroots/deps").exists(),
            ".gcroots must survive the clean — it holds the nix store roots"
        );
        assert!(
            !checkout.join("target/lib.rlib").exists(),
            "target must be swept — shared CARGO_TARGET_DIR replaces per-repo target"
        );
        assert!(
            !checkout.join("stale.log").exists(),
            "unrelated leftovers are still swept"
        );
    }
}
