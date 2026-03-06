//! Interactive REPL for direct conversation.
//!
//! Provides a simple stdin/stdout loop for chatting with the agent.
//! Acquires a session lock to prevent concurrent access.
//!
//! The I/O loop is thin: it reads a line, parses it into a [`Command`],
//! and dispatches. All parsing logic lives in [`Command::parse`] so it
//! can be tested without touching stdin/stdout.

use std::io::{self, Write};

use tracing::error;

use crate::agent;
use crate::commands::{self, SlashCommand};
use crate::config::ContextConfig;
use crate::lock::Lock;
use crate::provider::Provider;
use crate::session::Session;
use crate::tools::Tools;
use crate::workspace::Workspace;

/// Parsed user input.
#[derive(Debug, PartialEq, Eq)]
pub enum Command<'a> {
    /// Blank line — do nothing.
    Empty,
    /// `/exit` — end the session.
    Exit,
    /// A recognized slash command.
    Slash(SlashCommand),
    /// Send a message to the agent.
    Message(&'a str),
    /// Unrecognized `/` command.
    Unknown(&'a str),
}

impl<'a> Command<'a> {
    /// Parse a raw input line (including trailing newline) into a command.
    pub fn parse(input: &'a str) -> Self {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            Self::Empty
        } else if trimmed == "/exit" {
            Self::Exit
        } else if let Some(name) = trimmed.strip_prefix('/') {
            match SlashCommand::parse(name) {
                Some(cmd) => Self::Slash(cmd),
                None => Self::Unknown(trimmed),
            }
        } else {
            Self::Message(trimmed)
        }
    }
}

/// Run the interactive REPL loop.
///
/// Acquires the REPL lock, loads the session, and enters a read-eval-print
/// loop until the user sends EOF or types `/exit`.
pub async fn run<P: Provider>(
    workspace: &Workspace,
    provider: &P,
    tools: &Tools,
    max_iterations: usize,
    ctx: &ContextConfig,
) {
    let Ok(_lock) = Lock::acquire(&workspace.repl_lock_path()) else {
        error!("Another session is already running");
        std::process::exit(1);
    };

    let session_path = workspace.repl_session_path();

    let mut session = Session::load(&session_path).unwrap_or_else(|e| {
        error!("Failed to load session: {e}");
        std::process::exit(1);
    });

    let mut system_prompt = workspace.system_prompt();

    println!("{}\n", commands::greeting(session.messages().len()));

    loop {
        print!("> ");
        io::stdout().flush().unwrap();

        let mut input = String::new();
        match io::stdin().read_line(&mut input) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }

        match Command::parse(&input) {
            Command::Empty => {}
            Command::Exit => break,
            Command::Slash(cmd) => {
                let rebuild_prompt = cmd == SlashCommand::NewSession;
                match commands::execute(cmd, &mut session, &session_path, workspace, provider, ctx)
                    .await
                {
                    Ok(msg) => println!("{msg}\n"),
                    Err(msg) => eprintln!("{msg}\n"),
                }
                if rebuild_prompt {
                    system_prompt = workspace.system_prompt();
                }
            }
            Command::Unknown(cmd) => {
                println!("Unknown command: {cmd}\n");
            }
            Command::Message(msg) => {
                match agent::run_turn(
                    &mut session,
                    &system_prompt,
                    msg,
                    provider,
                    tools,
                    max_iterations,
                    ctx,
                )
                .await
                {
                    Ok(response) => {
                        println!("{response}\n");
                        if let Err(e) = session.save(&session_path) {
                            error!("Failed to save session: {e}");
                        }
                    }
                    Err(e) => error!("Error: {e}"),
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
        assert_eq!(Command::parse(""), Command::Empty);
        assert_eq!(Command::parse("   "), Command::Empty);
        assert_eq!(Command::parse("\n"), Command::Empty);
        assert_eq!(Command::parse("  \n"), Command::Empty);
    }

    #[test]
    fn parse_exit() {
        assert_eq!(Command::parse("/exit"), Command::Exit);
        assert_eq!(Command::parse("/exit\n"), Command::Exit);
        assert_eq!(Command::parse("  /exit  "), Command::Exit);
    }

    #[test]
    fn parse_new_session() {
        assert_eq!(
            Command::parse("/new"),
            Command::Slash(SlashCommand::NewSession)
        );
        assert_eq!(
            Command::parse("/new\n"),
            Command::Slash(SlashCommand::NewSession)
        );
    }

    #[test]
    fn parse_message() {
        assert_eq!(Command::parse("hello\n"), Command::Message("hello"));
        assert_eq!(
            Command::parse("  what is rust  \n"),
            Command::Message("what is rust")
        );
    }

    #[test]
    fn parse_exit_is_a_message() {
        assert_eq!(Command::parse("exit"), Command::Message("exit"));
        assert_eq!(Command::parse("exit now"), Command::Message("exit now"));
    }

    #[test]
    fn parse_unknown_slash_commands() {
        assert_eq!(Command::parse("/help"), Command::Unknown("/help"));
        assert_eq!(Command::parse("/nwe"), Command::Unknown("/nwe"));
        assert_eq!(Command::parse("  /foo  \n"), Command::Unknown("/foo"));
    }

    #[test]
    fn parse_slash_with_args_is_unknown() {
        assert_eq!(
            Command::parse("/new session"),
            Command::Unknown("/new session")
        );
    }

    #[test]
    fn parse_context() {
        assert_eq!(
            Command::parse("/context"),
            Command::Slash(SlashCommand::Context)
        );
    }

    #[test]
    fn parse_compact() {
        assert_eq!(
            Command::parse("/compact"),
            Command::Slash(SlashCommand::Compact)
        );
    }

    #[test]
    fn parse_stats() {
        assert_eq!(
            Command::parse("/stats"),
            Command::Slash(SlashCommand::Stats)
        );
    }
}
