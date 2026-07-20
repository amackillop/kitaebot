//! End-to-end tests: spawn the real daemon, connect with kchat.
//!
//! Requires the `mock-network` feature so the daemon uses the stub
//! provider instead of calling real APIs.

use std::path::Path;
use std::process::{Child, Command};
use std::time::Duration;

use predicates::prelude::*;
use tempfile::TempDir;

// ── Helpers ─────────────────────────────────────────────────────────

struct Daemon {
    child: Child,
}

impl Daemon {
    /// Spawn the daemon with a workspace and socket path, then wait for
    /// the socket to appear.
    fn spawn(workspace: &Path, socket_path: &Path) -> Self {
        Self::spawn_with_env(workspace, socket_path, &[])
    }

    /// Like [`Daemon::spawn`], with extra environment variables.
    fn spawn_with_env(workspace: &Path, socket_path: &Path, envs: &[(&str, &str)]) -> Self {
        let config = format!("[socket]\npath = \"{}\"\n", socket_path.display());
        std::fs::write(workspace.join("config.toml"), config).unwrap();

        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("kitaebot"));
        cmd.arg("run").env("KITAEBOT_WORKSPACE", workspace);
        for (key, value) in envs {
            cmd.env(key, value);
        }
        let child = cmd.spawn().expect("failed to spawn daemon");

        // Wait for the socket to appear.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !socket_path.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "daemon did not create socket within 5s"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        Self { child }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn kchat(socket_path: &Path) -> assert_cmd::Command {
    let mut cmd = assert_cmd::Command::new(assert_cmd::cargo::cargo_bin!("kchat"));
    cmd.arg(socket_path);
    cmd
}

// ── Tests ───────────────────────────────────────────────────────────

#[test]
fn daemon_greeting_and_message_roundtrip() {
    let ws_dir = TempDir::new().unwrap();
    let sock_dir = TempDir::new().unwrap();
    let sock_path = sock_dir.path().join("chat.sock");

    let _daemon = Daemon::spawn(ws_dir.path(), &sock_path);

    kchat(&sock_path)
        .write_stdin("hello\n/exit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("New session"))
        .stdout(predicate::str::contains("stub response"));
}

#[test]
fn daemon_slash_command() {
    let ws_dir = TempDir::new().unwrap();
    let sock_dir = TempDir::new().unwrap();
    let sock_path = sock_dir.path().join("chat.sock");

    let _daemon = Daemon::spawn(ws_dir.path(), &sock_path);

    // Send a message first to create session state, then clear it.
    kchat(&sock_path)
        .write_stdin("hello\n/new\n/exit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Session cleared."));
}

#[test]
fn daemon_turn_with_tool_call() {
    let ws_dir = TempDir::new().unwrap();
    let sock_dir = TempDir::new().unwrap();
    let sock_path = sock_dir.path().join("chat.sock");

    // Turn script: one exec tool call, then a final text response.
    let script = r#"[
        {"choices":[{"message":{"content":null,"tool_calls":[
            {"id":"call-1","function":{"name":"exec","arguments":"{\"command\":\"echo e2e-marker\"}"}}
        ]},"finish_reason":"tool_calls"}]},
        {"choices":[{"message":{"content":"tool turn complete"}}]}
    ]"#;
    let script_path = sock_dir.path().join("stub-responses.json");
    std::fs::write(&script_path, script).unwrap();

    let _daemon = Daemon::spawn_with_env(
        ws_dir.path(),
        &sock_path,
        &[("KITAEBOT_STUB_RESPONSES", script_path.to_str().unwrap())],
    );

    kchat(&sock_path)
        .write_stdin("run it\n/exit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("tool turn complete"))
        .stderr(predicate::str::contains("Running tool: exec"))
        .stderr(predicate::str::contains("Tool finished: exec"));
}

#[test]
fn daemon_session_persists_across_clients() {
    let ws_dir = TempDir::new().unwrap();
    let sock_dir = TempDir::new().unwrap();
    let sock_path = sock_dir.path().join("chat.sock");

    let _daemon = Daemon::spawn(ws_dir.path(), &sock_path);

    // First client: send a message to create session state.
    kchat(&sock_path)
        .write_stdin("hello\n/exit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("New session"));

    // Second client: should see resumed session.
    kchat(&sock_path)
        .write_stdin("/exit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Resumed:"));
}
