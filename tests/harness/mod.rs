//! Blackbox e2e harness: the real daemon binary against a loopback
//! fixture server, driven through kchat.

mod fixture;

pub use fixture::{FixtureServer, text, tool_call};

use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;

use tempfile::TempDir;

/// A running daemon on a temp workspace, killed on drop.
pub struct TestDaemon {
    child: Child,
    socket_path: PathBuf,
    _workspace: TempDir,
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
            _workspace: workspace,
            _sock_dir: sock_dir,
        }
    }

    /// A kchat command connected to this daemon's socket.
    pub fn kchat(&self) -> assert_cmd::Command {
        let mut cmd = assert_cmd::Command::new(assert_cmd::cargo::cargo_bin!("kchat"));
        cmd.arg(&self.socket_path);
        cmd
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
