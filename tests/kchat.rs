//! Integration tests for the kchat binary.
//!
//! Each test spins up a minimal NDJSON echo server on a Unix socket,
//! runs the kchat binary against it, and asserts on stdout/stderr.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use assert_cmd::Command;
use predicates::prelude::*;
use serde::{Deserialize, Serialize};

// ── Protocol types (mirrored from socket.rs) ────────────────────────

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMsg {
    Message { content: String },
    Command { name: String },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
enum ServerMsg {
    Greeting { content: String },
    Response { content: String },
    CommandResult { content: String },
    Error { content: String },
}

// ── Helpers ─────────────────────────────────────────────────────────

fn sock_path() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.sock");
    (dir, path)
}

fn kchat(path: &PathBuf) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("kchat"));
    cmd.arg(path);
    cmd
}

fn send_line(stream: &mut UnixStream, msg: &ServerMsg) {
    let mut buf = serde_json::to_string(msg).unwrap();
    buf.push('\n');
    stream.write_all(buf.as_bytes()).unwrap();
}

fn recv_line(reader: &mut BufReader<UnixStream>) -> ClientMsg {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

/// Spawn a mock server that accepts one client, sends a greeting,
/// then echoes each message back as a response. Runs the provided
/// handler in a background thread and returns the thread handle.
fn spawn_echo_server(
    path: &PathBuf,
    handler: impl FnOnce(UnixStream) + Send + 'static,
) -> std::thread::JoinHandle<()> {
    let listener = UnixListener::bind(path).unwrap();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        handler(stream);
    })
}

// ── Tests ───────────────────────────────────────────────────────────

#[test]
fn prints_greeting_and_exits_on_eof() {
    let (_dir, path) = sock_path();

    let server = spawn_echo_server(&path, |mut stream| {
        send_line(
            &mut stream,
            &ServerMsg::Greeting {
                content: "New session".into(),
            },
        );
        // Client sends EOF, server reads 0 bytes — done.
        let mut buf = [0u8; 1];
        let _ = std::io::Read::read(&mut stream, &mut buf);
    });

    kchat(&path)
        .write_stdin("")
        .assert()
        .success()
        .stdout(predicate::str::contains("New session"));

    server.join().unwrap();
}

#[test]
fn message_roundtrip() {
    let (_dir, path) = sock_path();

    let server = spawn_echo_server(&path, |stream| {
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);

        send_line(
            &mut writer,
            &ServerMsg::Greeting {
                content: "New session".into(),
            },
        );

        let msg = recv_line(&mut reader);
        assert!(matches!(msg, ClientMsg::Message { content } if content == "hello"));

        send_line(
            &mut writer,
            &ServerMsg::Response {
                content: "world".into(),
            },
        );

        // Client sends /exit locally, no more messages.
    });

    kchat(&path)
        .write_stdin("hello\n/exit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("New session"))
        .stdout(predicate::str::contains("world"));

    server.join().unwrap();
}

#[test]
fn slash_command_sent_as_command_type() {
    let (_dir, path) = sock_path();

    let server = spawn_echo_server(&path, |stream| {
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);

        send_line(
            &mut writer,
            &ServerMsg::Greeting {
                content: "New session".into(),
            },
        );

        let msg = recv_line(&mut reader);
        assert!(matches!(msg, ClientMsg::Command { name } if name == "new"));

        send_line(
            &mut writer,
            &ServerMsg::CommandResult {
                content: "Session cleared.".into(),
            },
        );
    });

    kchat(&path)
        .write_stdin("/new\n/exit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Session cleared."));

    server.join().unwrap();
}

#[test]
fn multiple_messages() {
    let (_dir, path) = sock_path();

    let server = spawn_echo_server(&path, |stream| {
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);

        send_line(
            &mut writer,
            &ServerMsg::Greeting {
                content: "New session".into(),
            },
        );

        for i in 1..=3 {
            let _msg = recv_line(&mut reader);
            send_line(
                &mut writer,
                &ServerMsg::Response {
                    content: format!("reply {i}"),
                },
            );
        }
    });

    kchat(&path)
        .write_stdin("a\nb\nc\n/exit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("reply 1"))
        .stdout(predicate::str::contains("reply 2"))
        .stdout(predicate::str::contains("reply 3"));

    server.join().unwrap();
}

#[test]
fn empty_lines_skipped() {
    let (_dir, path) = sock_path();

    let server = spawn_echo_server(&path, |stream| {
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);

        send_line(
            &mut writer,
            &ServerMsg::Greeting {
                content: "New session".into(),
            },
        );

        // Only one message should arrive (the empty lines are skipped).
        let _msg = recv_line(&mut reader);
        send_line(
            &mut writer,
            &ServerMsg::Response {
                content: "got it".into(),
            },
        );
    });

    kchat(&path)
        .write_stdin("\n\nhello\n/exit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("got it"));

    server.join().unwrap();
}

#[test]
fn connection_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nonexistent.sock");

    kchat(&path)
        .assert()
        .failure()
        .stderr(predicate::str::contains("Failed to connect"));
}

#[test]
fn no_args_prints_usage() {
    Command::new(assert_cmd::cargo::cargo_bin!("kchat"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("Usage: kchat"));
}
