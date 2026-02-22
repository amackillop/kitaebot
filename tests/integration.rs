//! Integration tests for the kitaebot binary.

use assert_cmd::Command;
use predicates::prelude::*;

fn kitaebot() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("kitaebot"))
}

#[test]
fn welcome_message() {
    kitaebot()
        .write_stdin("exit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Kitaebot REPL (stub mode)"))
        .stdout(predicate::str::contains("Type 'exit' to quit"));
}

#[test]
fn exit_command() {
    kitaebot().write_stdin("exit\n").assert().success();
}

#[test]
fn eof_exits_cleanly() {
    kitaebot().write_stdin("").assert().success();
}

#[test]
fn stub_response() {
    kitaebot()
        .write_stdin("hello\nexit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("This is a stub response"));
}

#[test]
fn multiple_inputs() {
    kitaebot()
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
    kitaebot()
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
    kitaebot()
        .write_stdin("test\nexit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains(">"));
}
