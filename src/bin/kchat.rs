//! Thin NDJSON client for the kitaebot Unix socket channel.
//!
//! Connects to a socket, prints the greeting, and enters a REPL.
//! All input is sent as `{"content": "..."}` — the server handles
//! slash command parsing. Exits on EOF or `/exit`.

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
    match recv(&mut reader) {
        ServerMsg::Greeting { content } => println!("{content}\n"),
        other => print_response(&other),
    }

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

        send(&mut writer, &ClientMsg { content: trimmed });

        // Read responses, printing activity lines to stderr until we
        // get the final Response or Error.
        loop {
            let msg = recv(&mut reader);
            match msg {
                ServerMsg::Activity { content } => {
                    eprintln!("  ~ {content}");
                }
                other => {
                    print_response(&other);
                    break;
                }
            }
        }
    }
}

fn print_response(msg: &ServerMsg) {
    match msg {
        ServerMsg::Response { content } | ServerMsg::Greeting { content } => {
            println!("{content}\n");
        }
        ServerMsg::Error { content } => {
            eprintln!("{content}\n");
        }
        ServerMsg::Activity { content } => {
            eprintln!("  ~ {content}");
        }
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
