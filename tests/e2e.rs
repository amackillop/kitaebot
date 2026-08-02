//! End-to-end tests: spawn the real daemon, connect with kchat.
//!
//! Requires the `mock-network` feature so client construction accepts
//! only loopback hosts; the harness points the daemon at a local
//! fixture server. Excluded from `nix flake check`; run with
//! `just test-e2e`.

mod harness;

use harness::{FixtureServer, TestDaemon, text, tool_call};
use predicates::prelude::*;
use serde_json::json;

#[test]
fn daemon_greeting_and_message_roundtrip() {
    let fixture = FixtureServer::start();
    fixture.on_completion("hello", text("hi from the fixture"));
    let daemon = TestDaemon::spawn(&fixture);

    daemon
        .kchat()
        .write_stdin("hello\n/exit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("New session"))
        .stdout(predicate::str::contains("hi from the fixture"));
}

#[test]
fn daemon_slash_command() {
    let fixture = FixtureServer::start();
    fixture.on_completion("hello", text("ack"));
    let daemon = TestDaemon::spawn(&fixture);

    // Send a message first to create session state, then clear it.
    daemon
        .kchat()
        .write_stdin("hello\n/new\n/exit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Session cleared."));
}

#[test]
fn daemon_turn_with_tool_call() {
    let fixture = FixtureServer::start();
    // First turn iteration asks for an exec call; the follow-up
    // request carries the tool output and gets the final text.
    fixture.on_completion(
        "run it",
        tool_call("exec", &json!({"command": "echo e2e-marker"})),
    );
    fixture.on_completion("e2e-marker", text("tool turn complete"));
    let daemon = TestDaemon::spawn(&fixture);

    daemon
        .kchat()
        .write_stdin("run it\n/exit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("tool turn complete"))
        .stderr(predicate::str::contains("Running tool: exec"))
        .stderr(predicate::str::contains("Tool finished: exec"));
}

// ── Telegram channel ────────────────────────────────────────────────

fn telegram_config(fixture: &FixtureServer, chat_id: i64) -> String {
    format!(
        "[telegram]\nenabled = true\nchat_id = {chat_id}\napi_base = \"{}\"\n",
        fixture.api_base(),
    )
}

#[test]
fn telegram_message_roundtrip() {
    let fixture = FixtureServer::start();
    fixture.on_completion("ping", text("pong"));
    let _daemon = TestDaemon::spawn_with(&fixture, &telegram_config(&fixture, 42));

    fixture.push_telegram_update(42, "ping");

    let sent = fixture.wait_for_telegram_send("pong");
    assert_eq!(sent["chat_id"], 42);
}

#[test]
fn telegram_ignores_foreign_chat() {
    let fixture = FixtureServer::start();
    fixture.on_completion("ping", text("pong"));
    let _daemon = TestDaemon::spawn_with(&fixture, &telegram_config(&fixture, 42));

    // The foreign message is queued first; updates are processed in
    // order, so once "pong" went out it has already been skipped.
    fixture.push_telegram_update(999, "intruder");
    fixture.push_telegram_update(42, "ping");

    fixture.wait_for_telegram_send("pong");
    assert!(
        fixture
            .telegram_sends()
            .iter()
            .all(|send| !send["text"].as_str().unwrap().contains("intruder")),
        "foreign chat message produced a send",
    );
}

#[test]
fn notify_tool_reaches_telegram() {
    let fixture = FixtureServer::start();
    // The turn arrives over the socket; the notification (low
    // urgency) is batched and flushed to Telegram after the turn.
    fixture.on_completion(
        "notify me",
        tool_call("notify", &serde_json::json!({"message": "deploy finished"})),
    );
    fixture.on_completion("Notification queued", text("done"));
    let daemon = TestDaemon::spawn_with(&fixture, &telegram_config(&fixture, 42));

    daemon
        .kchat()
        .write_stdin("notify me\n/exit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("done"));

    let sent = fixture.wait_for_telegram_send("deploy finished");
    assert_eq!(sent["chat_id"], 42);
}

#[test]
fn daemon_session_persists_across_clients() {
    let fixture = FixtureServer::start();
    fixture.on_completion("hello", text("first reply"));
    let daemon = TestDaemon::spawn(&fixture);

    // First client: send a message to create session state.
    daemon
        .kchat()
        .write_stdin("hello\n/exit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("New session"));

    // Second client: should see resumed session.
    daemon
        .kchat()
        .write_stdin("/exit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Resumed:"));
}
