//! Thin NDJSON client for the kitaebot Unix socket channel.
//!
//! Connects to a socket, prints the greeting, and enters a REPL.
//! All input is sent as `{"content": "..."}` — the server handles
//! slash command parsing. Exits on EOF or `/exit`.
//!
//! With a message argument (`kchat <socket> <message>`) it runs
//! one-shot: sends the single message, prints the reply to stdout,
//! and exits with the server's status. This is the scripting path.

use std::io::{self, BufRead, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ── Protocol types (mirrored from socket.rs) ────────────────────────

#[derive(Serialize)]
struct ClientMsg<'a> {
    content: &'a str,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMsg {
    Activity { content: String },
    Error { content: String },
    Greeting { content: String },
    Response { content: String },
}

// ── Main ────────────────────────────────────────────────────────────

fn main() {
    let (path, message) = parse_args();

    let stream = UnixStream::connect(&path).unwrap_or_else(|e| {
        eprintln!("Failed to connect to {}: {e}", path.display());
        std::process::exit(1);
    });

    let mut reader = io::BufReader::new(stream.try_clone().unwrap_or_else(|e| {
        eprintln!("Failed to clone stream: {e}");
        std::process::exit(1);
    }));
    let mut writer = stream;

    // The server always opens with a greeting. In the REPL it heads
    // the session; one-shot mode routes it to stderr so stdout carries
    // only the reply.
    match recv(&mut reader) {
        ServerMsg::Greeting { content } if message.is_none() => println!("{content}\n"),
        ServerMsg::Greeting { content } => eprintln!("{content}"),
        other => std::process::exit(print_final(&other)),
    }

    if let Some(content) = message {
        std::process::exit(exchange(&mut reader, &mut writer, &content));
    }

    repl(&mut reader, &mut writer);
}

/// Send one message and drain server frames until the final reply.
/// Activity frames stream to stderr; the reply's exit code is
/// returned (0 for a response, 1 for an error).
fn exchange(reader: &mut io::BufReader<UnixStream>, writer: &mut UnixStream, content: &str) -> i32 {
    send(writer, &ClientMsg { content });
    loop {
        match recv(reader) {
            ServerMsg::Activity { content } => eprintln!("  ~ {content}"),
            other => return print_final(&other),
        }
    }
}

fn repl(reader: &mut io::BufReader<UnixStream>, writer: &mut UnixStream) {
    let mut input = String::new();
    loop {
        print!("> ");
        let _ = io::stdout().flush();

        input.clear();
        match io::stdin().read_line(&mut input) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }

        let trimmed = input.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "/exit" {
            break;
        }

        exchange(reader, writer, trimmed);
    }
}

/// Print a terminal server frame and yield its exit code.
fn print_final(msg: &ServerMsg) -> i32 {
    match msg {
        ServerMsg::Response { content } | ServerMsg::Greeting { content } => {
            println!("{content}");
            0
        }
        ServerMsg::Error { content } => {
            eprintln!("{content}");
            1
        }
        ServerMsg::Activity { content } => {
            eprintln!("  ~ {content}");
            0
        }
    }
}

// ── Wire helpers ────────────────────────────────────────────────────

fn send(writer: &mut UnixStream, msg: &ClientMsg) {
    let mut buf = serde_json::to_string(msg).unwrap_or_else(|e| {
        eprintln!("Serialize error: {e}");
        std::process::exit(1);
    });
    buf.push('\n');
    writer.write_all(buf.as_bytes()).unwrap_or_else(|e| {
        eprintln!("Write error: {e}");
        std::process::exit(1);
    });
}

fn recv(reader: &mut io::BufReader<UnixStream>) -> ServerMsg {
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => {
            eprintln!("Server closed connection");
            std::process::exit(0);
        }
        Ok(_) => serde_json::from_str(&line).unwrap_or_else(|e| {
            eprintln!("Invalid server response: {e}");
            std::process::exit(1);
        }),
        Err(e) => {
            eprintln!("Read error: {e}");
            std::process::exit(1);
        }
    }
}

fn parse_args() -> (PathBuf, Option<String>) {
    let mut args = std::env::args_os().skip(1);
    let Some(path) = args.next() else {
        eprintln!("Usage: kchat <socket-path> [message]");
        std::process::exit(1);
    };
    let message = args.next().map(|m| m.to_string_lossy().into_owned());
    (PathBuf::from(path), message)
}
