//! Input classification and routing.
//!
//! Every channel funnels user text through [`dispatch`], which classifies
//! it as a slash command or agent message and routes accordingly. The
//! [`Input`] type makes the fork explicit in the type system.

use std::path::Path;

use crate::agent::{self, TurnConfig};
use crate::commands::{self, SlashCommand};
use crate::provider::Provider;
use crate::workspace::Workspace;

/// Classified user input.
pub enum Input<'a> {
    /// A recognized slash command (local operation).
    Command(SlashCommand),
    /// Free-text message for the agent.
    Message(&'a str),
}

/// Input starts with `/` but doesn't match any known command.
#[derive(Debug)]
pub struct UnknownCommand;

impl<'a> Input<'a> {
    /// Classify raw user text.
    ///
    /// Text not starting with `/` is a message. Text starting with `/`
    /// must match a known command or it's an error.
    pub fn parse(text: &'a str) -> Result<Self, UnknownCommand> {
        if !text.starts_with('/') {
            return Ok(Self::Message(text));
        }
        text.parse::<SlashCommand>()
            .map(Self::Command)
            .map_err(|_| UnknownCommand)
    }
}

/// Route user input to the appropriate handler.
///
/// Returns `Ok(response)` on success or `Err(message)` on failure,
/// both as displayable strings.
pub async fn dispatch<P: Provider>(
    input: &str,
    session_path: &Path,
    workspace: &Workspace,
    config: &TurnConfig<'_, P>,
) -> Result<String, String> {
    match Input::parse(input).map_err(|_| format!("Unknown command: {input}"))? {
        Input::Command(cmd) => {
            commands::execute(
                cmd,
                session_path,
                workspace,
                config.provider,
                config.context,
            )
            .await
        }
        Input::Message(text) => agent::process_message(session_path, workspace, text, config)
            .await
            .map_err(|e| e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_without_slash() {
        let input = Input::parse("hello").unwrap();
        assert!(matches!(input, Input::Message("hello")));
    }

    #[test]
    fn known_command() {
        let input = Input::parse("/new").unwrap();
        assert!(matches!(input, Input::Command(SlashCommand::New)));
    }

    #[test]
    fn unknown_command() {
        assert!(Input::parse("/bogus").is_err());
    }

    #[test]
    fn empty_string_is_message() {
        let input = Input::parse("").unwrap();
        assert!(matches!(input, Input::Message("")));
    }
}
