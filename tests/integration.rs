//! Integration tests for the kitaebot binary.

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn kitaebot_with_workspace(dir: &TempDir) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("kitaebot"));
    cmd.env("KITAEBOT_WORKSPACE", dir.path());
    cmd
}

#[test]
fn welcome_message() {
    let dir = tempfile::tempdir().unwrap();
    kitaebot_with_workspace(&dir)
        .write_stdin("exit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Kitaebot REPL"))
        .stdout(predicate::str::contains("Type 'exit' to quit"));
}

#[test]
fn exit_command() {
    let dir = tempfile::tempdir().unwrap();
    kitaebot_with_workspace(&dir)
        .write_stdin("exit\n")
        .assert()
        .success();
}

#[test]
fn eof_exits_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    kitaebot_with_workspace(&dir)
        .write_stdin("")
        .assert()
        .success();
}

#[test]
fn stub_response() {
    let dir = tempfile::tempdir().unwrap();
    kitaebot_with_workspace(&dir)
        .write_stdin("hello\nexit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("This is a stub response"));
}

#[test]
fn multiple_inputs() {
    let dir = tempfile::tempdir().unwrap();
    kitaebot_with_workspace(&dir)
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
    kitaebot_with_workspace(&dir)
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
    kitaebot_with_workspace(&dir)
        .write_stdin("test\nexit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains(">"));
}

#[test]
fn workspace_initialized_on_start() {
    let dir = tempfile::tempdir().unwrap();
    kitaebot_with_workspace(&dir)
        .write_stdin("exit\n")
        .assert()
        .success();

    assert!(dir.path().join("SOUL.md").exists());
    assert!(dir.path().join("AGENTS.md").exists());
    assert!(dir.path().join("memory").is_dir());
    assert!(dir.path().join("projects").is_dir());
}

#[test]
fn session_persisted_after_turn() {
    let dir = tempfile::tempdir().unwrap();
    kitaebot_with_workspace(&dir)
        .write_stdin("hello\nexit\n")
        .assert()
        .success();

    assert!(dir.path().join("session.json").exists());
}

#[test]
fn new_command_clears_session() {
    let dir = tempfile::tempdir().unwrap();
    kitaebot_with_workspace(&dir)
        .write_stdin("hello\n/new\nexit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Session cleared."));
}
