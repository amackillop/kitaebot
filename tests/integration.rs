//! Integration tests for the kitaebot binary.

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn kitaebot_with_workspace(dir: &TempDir) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("kitaebot"));
    cmd.env("KITAEBOT_WORKSPACE", dir.path());
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
        .stderr(predicate::str::contains("heartbeat"));
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

#[test]
fn workspace_initialized_on_start() {
    let dir = tempfile::tempdir().unwrap();
    // heartbeat skips cleanly (no HEARTBEAT.md) but still inits workspace.
    kitaebot_with_workspace(&dir)
        .arg("heartbeat")
        .assert()
        .success();

    assert!(dir.path().join("sessions").is_dir());
    assert!(dir.path().join("locks").is_dir());
    assert!(dir.path().join("memory").is_dir());
    assert!(dir.path().join("projects").is_dir());
}
