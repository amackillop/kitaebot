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

use crate::agent;
use crate::commands::{self, ParseError, SlashCommand};
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
    /// Exit the session
    Exit,
    /// Send a message to the agent.
    Message(&'a str),
    /// A recognized slash command.
    Slash(SlashCommand),
    /// An unrecognized command.
    UnknownSlash(&'a str),
}

impl<'a> From<&'a str> for Command<'a> {
    fn from(input: &'a str) -> Self {
        let input = input.trim();
        match input {
            "" => Self::Empty,
            "/exit" => Self::Exit,
            _ => match input.parse() {
                Ok(cmd) => Self::Slash(cmd),
                Err(e) => match e {
                    ParseError::MustStartWithSlash => Self::Message(input),
                    ParseError::UnknownCommand => Self::UnknownSlash(input),
                },
            },
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

        match Command::from(input.as_str()) {
            Command::Empty => {}
            Command::Exit => break,
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
            Command::Slash(cmd) => {
                let rebuild_prompt = cmd == SlashCommand::New;
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
            Command::UnknownSlash(cmd) => {
                println!("Unknown command: {cmd}\n");
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
    fn parse_new_session() {
        assert_eq!(Command::from("/new"), Command::Slash(SlashCommand::New));
        assert_eq!(Command::from("/new\n"), Command::Slash(SlashCommand::New));
    }

    #[test]
    fn parse_message() {
        assert_eq!(Command::from("hello\n"), Command::Message("hello"));
        assert_eq!(
            Command::from("  what is rust  \n"),
            Command::Message("what is rust")
        );
    }

    #[test]
    fn parse_exit_is_a_message() {
        assert_eq!(Command::from("exit"), Command::Message("exit"));
        assert_eq!(Command::from("exit now"), Command::Message("exit now"));
    }

    #[test]
    fn parse_unknown_slash_commands() {
        assert_eq!(Command::from("/nwe"), Command::UnknownSlash("/nwe"));
        assert_eq!(Command::from("  /foo  \n"), Command::UnknownSlash("/foo"));
        assert_eq!(Command::from("//new"), Command::UnknownSlash("//new"));
    }

    #[test]
    fn parse_slash_with_args_is_unknown() {
        assert_eq!(
            Command::from("/new session"),
            Command::UnknownSlash("/new session")
        );
    }

    #[test]
    fn parse_context() {
        assert_eq!(
            Command::from("/context"),
            Command::Slash(SlashCommand::Context)
        );
    }

    #[test]
    fn parse_compact() {
        assert_eq!(
            Command::from("/compact"),
            Command::Slash(SlashCommand::Compact)
        );
    }

    #[test]
    fn parse_stats() {
        assert_eq!(Command::from("/stats"), Command::Slash(SlashCommand::Stats));
    }
}
