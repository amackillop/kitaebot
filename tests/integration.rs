//! Integration tests for the kitaebot binary.

use std::fs;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn kitaebot_with_workspace(dir: &TempDir) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("kitaebot"));
    cmd.env("KITAEBOT_WORKSPACE", dir.path());
    cmd
}

fn kitaebot_chat(dir: &TempDir) -> Command {
    let mut cmd = kitaebot_with_workspace(dir);
    cmd.arg("chat");
    cmd
}

// --- CLI dispatch tests ---

#[test]
fn bare_invocation_prints_usage() {
    let dir = tempfile::tempdir().unwrap();
    kitaebot_with_workspace(&dir)
        .assert()
        .failure()
        .stderr(predicate::str::contains("Usage: kitaebot <command>"))
        .stderr(predicate::str::contains("chat"));
}

#[test]
fn unknown_subcommand_fails() {
    let dir = tempfile::tempdir().unwrap();
    kitaebot_with_workspace(&dir)
        .arg("bogus")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Unknown command: bogus"));
}

// --- Chat REPL tests ---

#[test]
fn new_session_status() {
    let dir = tempfile::tempdir().unwrap();
    kitaebot_chat(&dir)
        .write_stdin("exit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("New session"));
}

#[test]
fn resumed_session_status() {
    let dir = tempfile::tempdir().unwrap();

    // First run: create a session with one turn
    kitaebot_chat(&dir)
        .write_stdin("hello\nexit\n")
        .assert()
        .success();

    // Second run: should show resumed
    kitaebot_chat(&dir)
        .write_stdin("exit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Resumed session"));
}

#[test]
fn exit_command() {
    let dir = tempfile::tempdir().unwrap();
    kitaebot_chat(&dir).write_stdin("exit\n").assert().success();
}

#[test]
fn eof_exits_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    kitaebot_chat(&dir).write_stdin("").assert().success();
}

#[test]
fn stub_response() {
    let dir = tempfile::tempdir().unwrap();
    kitaebot_chat(&dir)
        .write_stdin("hello\nexit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("This is a stub response"));
}

#[test]
fn multiple_inputs() {
    let dir = tempfile::tempdir().unwrap();
    kitaebot_chat(&dir)
        .write_stdin("first\nsecond\nthird\nexit\n")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("This is a stub response")
                .count(3)
                .from_utf8(),
        );
}

#[test]
fn empty_input_skipped() {
    let dir = tempfile::tempdir().unwrap();
    kitaebot_chat(&dir)
        .write_stdin("\n\nhello\nexit\n")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("This is a stub response")
                .count(1)
                .from_utf8(),
        );
}

#[test]
fn prompts_displayed() {
    let dir = tempfile::tempdir().unwrap();
    kitaebot_chat(&dir)
        .write_stdin("test\nexit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains(">"));
}

#[test]
fn workspace_initialized_on_start() {
    let dir = tempfile::tempdir().unwrap();
    kitaebot_chat(&dir).write_stdin("exit\n").assert().success();

    assert!(dir.path().join("SOUL.md").exists());
    assert!(dir.path().join("AGENTS.md").exists());
    assert!(dir.path().join("memory").is_dir());
    assert!(dir.path().join("projects").is_dir());
}

#[test]
fn session_persisted_after_turn() {
    let dir = tempfile::tempdir().unwrap();
    kitaebot_chat(&dir)
        .write_stdin("hello\nexit\n")
        .assert()
        .success();

    assert!(dir.path().join("session.json").exists());
}

#[test]
fn new_command_clears_session() {
    let dir = tempfile::tempdir().unwrap();
    kitaebot_chat(&dir)
        .write_stdin("hello\n/new\nexit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Session cleared."));
}

// --- Heartbeat integration tests ---

#[test]
fn heartbeat_no_file_skips() {
    let dir = tempfile::tempdir().unwrap();
    kitaebot_with_workspace(&dir)
        .arg("heartbeat")
        .assert()
        .success()
        .stderr(predicate::str::contains("no HEARTBEAT.md"));
}

#[test]
fn heartbeat_no_tasks_skips() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("HEARTBEAT.md"),
        "# Heartbeat\n\n- [x] Already done\n",
    )
    .unwrap();

    kitaebot_with_workspace(&dir)
        .arg("heartbeat")
        .assert()
        .success()
        .stderr(predicate::str::contains("no active tasks"));
}

#[test]
fn heartbeat_with_tasks_executes() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("HEARTBEAT.md"),
        "# Heartbeat\n\n- [ ] Test task\n",
    )
    .unwrap();

    kitaebot_with_workspace(&dir)
        .arg("heartbeat")
        .assert()
        .success()
        .stderr(predicate::str::contains("Heartbeat complete"));

    assert!(dir.path().join("memory/HISTORY.md").exists());
}

#[test]
fn heartbeat_skips_when_repl_locked() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("HEARTBEAT.md"),
        "# Heartbeat\n\n- [ ] Test task\n",
    )
    .unwrap();

    // Simulate a REPL holding the lock.
    fs::write(dir.path().join("repl.lock"), std::process::id().to_string()).unwrap();

    kitaebot_with_workspace(&dir)
        .arg("heartbeat")
        .assert()
        .success()
        .stderr(predicate::str::contains("user session active"));

    // History should not be written.
    assert!(!dir.path().join("memory/HISTORY.md").exists());
}
