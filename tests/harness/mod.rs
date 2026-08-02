//! Blackbox e2e harness: the real daemon binary against a loopback
//! fixture server, driven through kchat.

mod fixture;

pub use fixture::{
    FixtureServer, github_pr, github_review, linear_comment, linear_issue, text, tool_call,
};

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

use tempfile::TempDir;

/// A running daemon on a temp workspace, killed on drop.
pub struct TestDaemon {
    child: Child,
    socket_path: PathBuf,
    workspace: TempDir,
    _sock_dir: TempDir,
}

impl TestDaemon {
    /// Spawn the daemon wired to `fixture` for completions.
    pub fn spawn(fixture: &FixtureServer) -> Self {
        Self::spawn_with(fixture, "")
    }

    /// Like [`TestDaemon::spawn`], with extra config.toml sections
    /// (e.g. a `[telegram]` block pointing at the fixture).
    pub fn spawn_with(fixture: &FixtureServer, extra_config: &str) -> Self {
        let workspace = TempDir::new().unwrap();
        let sock_dir = TempDir::new().unwrap();
        let socket_path = sock_dir.path().join("chat.sock");

        let config = format!(
            "[socket]\npath = \"{}\"\n\n[provider]\napi = \"{}\"\n\n{extra_config}",
            socket_path.display(),
            fixture.completions_url(),
        );
        std::fs::write(workspace.path().join("config.toml"), config).unwrap();

        let child = Command::new(assert_cmd::cargo::cargo_bin!("kitaebot"))
            .arg("run")
            .env("KITAEBOT_WORKSPACE", workspace.path())
            // Hermetic HOME: git subprocesses inherit it via safe_env,
            // so the host's git config never leaks into e2e runs.
            .env("HOME", workspace.path())
            .spawn()
            .expect("failed to spawn daemon");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !socket_path.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "daemon did not create socket within 5s"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        Self {
            child,
            socket_path,
            workspace,
            _sock_dir: sock_dir,
        }
    }

    /// A kchat command connected to this daemon's socket.
    pub fn kchat(&self) -> assert_cmd::Command {
        let mut cmd = assert_cmd::Command::new(assert_cmd::cargo::cargo_bin!("kchat"));
        cmd.arg(&self.socket_path);
        cmd
    }

    /// The daemon's workspace root, for asserting on files it writes.
    pub fn workspace_path(&self) -> &Path {
        self.workspace.path()
    }
}

// ── Git fixture repos ───────────────────────────────────────────────

/// Run git in `dir`, panicking on failure. Identity via -c flags so
/// the fixture works without global git config.
fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
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

/// Create `<root>/<owner>/<repo>.git` with a `main` commit and a PR
/// commit reachable via `refs/pull/<n>/head`. Point `git.clone_base`
/// at `file://<root>` to serve it. Returns the PR head SHA.
pub fn git_fixture_pr_repo(root: &Path, nwo: &str, pr_number: u32) -> String {
    let dir = root.join(format!("{nwo}.git"));
    std::fs::create_dir_all(&dir).unwrap();
    git_in(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("a.txt"), "base\n").unwrap();
    git_in(&dir, &["add", "a.txt"]);
    git_in(&dir, &["commit", "-m", "base"]);
    git_in(&dir, &["checkout", "-b", "pr-branch"]);
    std::fs::write(dir.join("a.txt"), "pr change\n").unwrap();
    git_in(&dir, &["commit", "-am", "pr change"]);
    let sha = git_in(&dir, &["rev-parse", "HEAD"]).trim().to_string();
    git_in(
        &dir,
        &["update-ref", &format!("refs/pull/{pr_number}/head"), &sha],
    );
    git_in(&dir, &["checkout", "main"]);
    sha
}

/// Add a commit to the fixture PR branch and advance its pull ref,
/// simulating a push. Returns the new head SHA.
pub fn git_fixture_push(root: &Path, nwo: &str, pr_number: u32) -> String {
    let dir = root.join(format!("{nwo}.git"));
    git_in(&dir, &["checkout", "pr-branch"]);
    std::fs::write(dir.join("a.txt"), "pr v2\n").unwrap();
    git_in(&dir, &["commit", "-am", "pr v2"]);
    let sha = git_in(&dir, &["rev-parse", "HEAD"]).trim().to_string();
    git_in(
        &dir,
        &["update-ref", &format!("refs/pull/{pr_number}/head"), &sha],
    );
    git_in(&dir, &["checkout", "main"]);
    sha
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
