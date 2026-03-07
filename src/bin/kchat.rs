//! Thin NDJSON client for the kitaebot Unix socket channel.
//!
//! Connects to a socket, prints the greeting, and enters a REPL:
//! lines starting with `/` become command messages, everything else
//! becomes chat messages. Exits on EOF or `/exit`.

use std::io::{self, BufRead, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ── Protocol types (mirrored from socket.rs) ────────────────────────

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMsg<'a> {
    Message { content: &'a str },
    Command { name: &'a str },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMsg {
    Greeting { content: String },
    Response { content: String },
    CommandResult { content: String },
    Error { content: String },
}

impl ServerMsg {
    fn content(&self) -> &str {
        match self {
            Self::Greeting { content }
            | Self::Response { content }
            | Self::CommandResult { content }
            | Self::Error { content } => content,
        }
    }
}

// ── Main ────────────────────────────────────────────────────────────

fn main() {
    let path = parse_args();

    let stream = UnixStream::connect(&path).unwrap_or_else(|e| {
        eprintln!("Failed to connect to {}: {e}", path.display());
        std::process::exit(1);
    });

    let mut reader = io::BufReader::new(stream.try_clone().unwrap_or_else(|e| {
        eprintln!("Failed to clone stream: {e}");
        std::process::exit(1);
    }));
    let mut writer = stream;

    // Read and print greeting.
    let greeting = recv(&mut reader);
    println!("{}\n", greeting.content());

    // REPL loop.
    let mut input = String::new();
    loop {
        print!("> ");
        io::stdout().flush().unwrap();

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

        let msg = if let Some(name) = trimmed.strip_prefix('/') {
            ClientMsg::Command { name }
        } else {
            ClientMsg::Message { content: trimmed }
        };

        send(&mut writer, &msg);
        let response = recv(&mut reader);
        println!("{}\n", response.content());
    }
}

// ── Wire helpers ────────────────────────────────────────────────────

fn send(writer: &mut UnixStream, msg: &ClientMsg) {
    let mut buf = serde_json::to_string(msg).expect("ClientMsg is always serializable");
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

fn parse_args() -> PathBuf {
    let mut args = std::env::args_os().skip(1);
    if let Some(path) = args.next() {
        PathBuf::from(path)
    } else {
        eprintln!("Usage: kchat <socket-path>");
        std::process::exit(1);
    }
}
