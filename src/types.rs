//! Core domain types for the agent protocol.
//!
//! These are internal domain types, decoupled from any wire format.
//! Wire-format types for the `OpenAI` Chat Completions API live in
//! [`crate::provider::wire`].

use serde::{Deserialize, Serialize};

/// Message in the conversation history.
///
/// Represents one turn in the conversation between user, assistant, and tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    /// Assistant message containing either text or tool call requests.
    Assistant {
        content: String,
        /// Tool calls requested by the assistant. Empty if text-only response.
        tool_calls: Vec<ToolCall>,
    },

    /// System message containing instructions and context.
    System { content: String },

    /// Tool execution result message.
    Tool {
        /// ID of the tool call this result corresponds to.
        call_id: String,
        content: String,
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
    pub name: String,

    /// JSON string of arguments to pass to the tool
    pub arguments: String,
}

impl Message {
    /// Total character count across all content fields.
    ///
    /// Used for token estimation (`chars / 4`). Counts content strings
    /// and, for assistant messages, tool call function names + arguments.
    pub fn char_count(&self) -> usize {
        match self {
            Message::Assistant {
                content,
                tool_calls,
            } => {
                let base = content.len();
                let calls: usize = tool_calls
                    .iter()
                    .map(|tc| tc.function.name.len() + tc.function.arguments.len())
                    .sum();
                base + calls
            }
            Message::System { content }
            | Message::Tool { content, .. }
            | Message::User { content } => content.len(),
        }
    }
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
            tool_calls: vec![],
        };
        assert_eq!(msg.char_count(), 8);
    }

    #[test]
    fn char_count_assistant_with_tool_calls() {
        let msg = Message::Assistant {
            content: "ok".to_string(),
            tool_calls: vec![ToolCall::new(
                "id".to_string(),
                ToolFunction {
                    name: "exec".to_string(),                 // 4
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
}
