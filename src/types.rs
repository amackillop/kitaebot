//! Core domain types for the agent protocol.
//!
//! These are internal domain types, decoupled from any wire format.
//! Wire-format types for the `OpenAI` Chat Completions API live in
//! [`crate::provider::wire`].

use serde::{Deserialize, Serialize};

use crate::error::InvalidToolName;

/// Message in the conversation history.
///
/// Represents one turn in the conversation between user, assistant, and tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    /// Assistant text response (no tool calls).
    Assistant { content: String },

    /// System message containing instructions and context.
    System { content: String },

    /// Tool execution result message.
    Tool {
        /// ID of the tool call this result corresponds to.
        call_id: String,
        content: String,
    },

    /// Assistant response requesting tool execution.
    ToolCalls {
        content: String,
        calls: Vec<ToolCall>,
    },

    /// User message containing the input request.
    User { content: String },
}

/// A request from the LLM to execute a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub function: ToolFunction,
}

impl ToolCall {
    pub fn new(id: String, function: ToolFunction) -> Self {
        Self { id, function }
    }
}

/// Function details within a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunction {
    /// Name of the tool to execute
    pub name: ToolName,

    /// JSON string of arguments to pass to the tool
    pub arguments: String,
}

/// True when `name` satisfies the tool-name grammar every OpenAI-shaped
/// endpoint enforces: `^[a-zA-Z0-9_-]+$`.
pub fn is_valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// A tool name that the API will carry.
///
/// Every constructor validates against [`is_valid_tool_name`],
/// including the `Deserialize` impl, so a name that would 400 the
/// request cannot be built, parsed, or reloaded from a session file.
/// The grammar is the API's, not ours: a violation is untransmittable
/// rather than merely unknown, and one that reaches stored history
/// poisons every later request that replays it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct ToolName(String);

impl ToolName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for ToolName {
    type Err = InvalidToolName;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if is_valid_tool_name(s) {
            Ok(Self(s.to_string()))
        } else {
            Err(InvalidToolName(s.to_string()))
        }
    }
}

impl TryFrom<String> for ToolName {
    type Error = InvalidToolName;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        if is_valid_tool_name(&s) {
            Ok(Self(s))
        } else {
            Err(InvalidToolName(s))
        }
    }
}

impl std::fmt::Display for ToolName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<str> for ToolName {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for ToolName {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl Message {
    /// The text content common to every variant.
    pub fn content(&self) -> &str {
        match self {
            Self::Assistant { content }
            | Self::System { content }
            | Self::Tool { content, .. }
            | Self::ToolCalls { content, .. }
            | Self::User { content } => content,
        }
    }

    /// Total character count across all content fields.
    ///
    /// Used for token estimation (`chars / 4`). Counts content strings
    /// and, for tool-call messages, function names + arguments.
    pub fn char_count(&self) -> usize {
        match self {
            Self::ToolCalls { content, calls } => {
                let base = content.len();
                let extra: usize = calls
                    .iter()
                    .map(|tc| tc.function.name.as_str().len() + tc.function.arguments.len())
                    .sum();
                base + extra
            }
            _ => self.content().len(),
        }
    }

    /// Estimated token count for this message via [`estimate_tokens_from_chars`].
    pub fn token_estimate(&self) -> usize {
        estimate_tokens_from_chars(self.char_count())
    }
}

/// Estimate a token count from a character count (`chars / 4`).
///
/// Callers that sum characters across several sources should divide
/// once through this function rather than summing per-source estimates,
/// to avoid accumulating flooring error.
pub fn estimate_tokens_from_chars(chars: usize) -> usize {
    chars / 4
}

/// Estimate the token count of a string via the `chars / 4` heuristic.
///
/// This is the single token estimator used across the codebase (engines,
/// compaction, LCM tools). It undercounts structure-heavy content like
/// JSON but is cheap and consistent; observed `prompt_tokens` from the
/// provider corrects for drift where it matters (see spec 14).
pub fn estimate_tokens(s: &str) -> usize {
    estimate_tokens_from_chars(s.len())
}

/// LLM response - either final text or tool call requests.
///
/// The agent loop handles these differently:
/// - `Text`: Return to user and end turn
/// - `ToolCalls`: Execute tools and continue loop
#[derive(Debug, Clone)]
pub enum Response {
    /// Final text response to return to the user
    Text(String),

    /// One or more tool calls to execute, with optional accompanying text
    ToolCalls {
        content: String,
        calls: Vec<ToolCall>,
    },
}

/// Tool definition sent to the LLM.
///
/// Describes what the tool does and what arguments it accepts.
/// The LLM uses this to decide when and how to call tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Always "function" for function tools
    #[serde(rename = "type")]
    pub tool_type: String,

    /// Function specification
    pub function: FunctionDefinition,
}

/// Function specification within a tool definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDefinition {
    /// Name of the function (must match tool registry)
    pub name: String,

    /// Human-readable description of what the tool does
    pub description: String,

    /// JSON Schema describing the function's parameters
    pub parameters: serde_json::Value,
}

impl ToolDefinition {
    /// Create a new tool definition.
    ///
    /// # Arguments
    /// * `name` - Tool name (e.g., "exec")
    /// * `description` - What the tool does (e.g., "Execute a shell command")
    /// * `parameters` - JSON Schema for arguments
    pub fn new(name: String, description: String, parameters: serde_json::Value) -> Self {
        Self {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name,
                description,
                parameters,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_count_system() {
        let msg = Message::System {
            content: "hello".to_string(),
        };
        assert_eq!(msg.char_count(), 5);
    }

    #[test]
    fn char_count_user() {
        let msg = Message::User {
            content: "abc".to_string(),
        };
        assert_eq!(msg.char_count(), 3);
    }

    #[test]
    fn char_count_tool() {
        let msg = Message::Tool {
            call_id: "id_ignored".to_string(),
            content: "result".to_string(),
        };
        assert_eq!(msg.char_count(), 6);
    }

    #[test]
    fn char_count_assistant_text_only() {
        let msg = Message::Assistant {
            content: "response".to_string(),
        };
        assert_eq!(msg.char_count(), 8);
    }

    #[test]
    fn char_count_tool_calls() {
        let msg = Message::ToolCalls {
            content: "ok".to_string(),
            calls: vec![ToolCall::new(
                "id".to_string(),
                ToolFunction {
                    name: "exec".parse().unwrap(),            // 4
                    arguments: r#"{"cmd":"ls"}"#.to_string(), // 12
                },
            )],
        };
        // "ok" (2) + "exec" (4) + arguments (12) = 18
        assert_eq!(msg.char_count(), 18);
    }

    #[test]
    fn char_count_empty_message() {
        let msg = Message::User {
            content: String::new(),
        };
        assert_eq!(msg.char_count(), 0);
    }

    #[test]
    fn tool_name_accepts_the_grammar() {
        // Namespaced MCP tools carry both `_` and `-`.
        for ok in ["exec", "bkb_lookup-bip", "a", "A1_-"] {
            assert!(ok.parse::<ToolName>().is_ok(), "{ok} should parse");
        }
    }

    #[test]
    fn tool_name_rejects_everything_else() {
        // The middle entry is what Azure actually emitted.
        for bad in [
            "",
            "exec ",
            "group.tool",
            "server:tool",
            "review_disposition</arg_key><arg_value>fixed</arg_value>",
        ] {
            assert!(bad.parse::<ToolName>().is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn invalid_tool_name_error_names_the_offender() {
        let err = "a b".parse::<ToolName>().unwrap_err();
        assert!(err.to_string().contains("a b"));
    }

    /// The flat engine reloads history straight from JSON, so this is
    /// the only thing standing between a hand-edited or restored
    /// session file and a request the API refuses.
    #[test]
    fn deserializing_rejects_a_malformed_name() {
        let json = r#"{"name":"exec</arg_key>","arguments":"{}"}"#;
        assert!(serde_json::from_str::<ToolFunction>(json).is_err());
    }

    #[test]
    fn tool_function_round_trips_through_json() {
        let original = ToolFunction {
            name: "exec".parse().unwrap(),
            arguments: r#"{"command":"ls"}"#.to_string(),
        };
        let json = serde_json::to_string(&original).unwrap();
        // Serializes as a bare string, not a wrapper object: the stored
        // shape is unchanged, so existing sessions still load.
        assert!(json.contains(r#""name":"exec""#));
        let back: ToolFunction = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "exec");
    }
}
