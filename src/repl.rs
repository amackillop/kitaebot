//! Interactive REPL for direct conversation.
//!
//! Provides a simple stdin/stdout loop for chatting with the agent.
//! Acquires a session lock to prevent concurrent access.
//!
//! The I/O loop is thin: it reads a line, parses it into a [`Command`],
//! and dispatches. All parsing logic lives in [`Command::from`] so it
//! can be tested without touching stdin/stdout.

use std::io::{self, Write};

use tracing::error;

use crate::agent::TurnConfig;
use crate::commands;
use crate::dispatch;
use crate::lock::Lock;
use crate::provider::Provider;
use crate::workspace::Workspace;

/// Parsed user input.
#[derive(Debug, PartialEq, Eq)]
pub enum Command<'a> {
    /// Blank line — do nothing.
    Empty,
    /// Exit the session.
    Exit,
    /// Text to dispatch (message or slash command).
    Input(&'a str),
}

impl<'a> From<&'a str> for Command<'a> {
    fn from(input: &'a str) -> Self {
        let input = input.trim();
        match input {
            "" => Self::Empty,
            "/exit" => Self::Exit,
            _ => Self::Input(input),
        }
    }
}

/// Run the interactive REPL loop.
///
/// Acquires the REPL lock, loads the session, and enters a read-eval-print
/// loop until the user sends EOF or types `/exit`.
pub async fn run<P: Provider>(workspace: &Workspace, config: &TurnConfig<'_, P>) {
    let Ok(_lock) = Lock::acquire(&workspace.repl_lock_path()) else {
        error!("Another session is already running");
        std::process::exit(1);
    };

    let session_path = workspace.repl_session_path();

    println!("{}\n", commands::greeting(&session_path));

    loop {
        print!("> ");
        io::stdout().flush().unwrap();

        let mut input = String::new();
        match io::stdin().read_line(&mut input) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }

        match Command::from(input.as_str()) {
            Command::Empty => {}
            Command::Exit => break,
            Command::Input(text) => {
                match dispatch::dispatch(text, &session_path, workspace, config).await {
                    Ok(msg) => println!("{msg}\n"),
                    Err(msg) => eprintln!("{msg}\n"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_input() {
        assert_eq!(Command::from(""), Command::Empty);
        assert_eq!(Command::from("   "), Command::Empty);
        assert_eq!(Command::from("\n"), Command::Empty);
        assert_eq!(Command::from("  \n"), Command::Empty);
    }

    #[test]
    fn parse_exit() {
        assert_eq!(Command::from("/exit"), Command::Exit);
        assert_eq!(Command::from("/exit\n"), Command::Exit);
        assert_eq!(Command::from("  /exit  "), Command::Exit);
    }

    #[test]
    fn parse_message() {
        assert_eq!(Command::from("hello\n"), Command::Input("hello"));
        assert_eq!(
            Command::from("  what is rust  \n"),
            Command::Input("what is rust")
        );
    }

    #[test]
    fn parse_exit_without_slash_is_input() {
        assert_eq!(Command::from("exit"), Command::Input("exit"));
        assert_eq!(Command::from("exit now"), Command::Input("exit now"));
    }

    #[test]
    fn parse_slash_commands_are_input() {
        assert_eq!(Command::from("/new"), Command::Input("/new"));
        assert_eq!(Command::from("/context\n"), Command::Input("/context"));
        assert_eq!(Command::from("/compact"), Command::Input("/compact"));
        assert_eq!(Command::from("/stats"), Command::Input("/stats"));
        assert_eq!(Command::from("/nwe"), Command::Input("/nwe"));
        assert_eq!(
            Command::from("/new session"),
            Command::Input("/new session")
        );
    }
}
