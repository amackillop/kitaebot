//! Core domain types for the agent protocol.
//!
//! These types follow the `OpenAI` Chat Completions API format, which is also
//! compatible with `OpenRouter` and other OpenAI-compatible providers.
//!
//! See: <https://platform.openai.com/docs/api-reference/chat>

#![allow(dead_code)] // Types defined here will be used in later commits

use serde::{Deserialize, Serialize};

/// Message in the conversation history.
///
/// Represents one turn in the conversation between user, assistant, and tools.
/// Uses tagged enum serialization where the `role` field determines the variant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    /// System message containing instructions and context.
    ///
    /// Typically includes SOUL.md, AGENTS.md, and tool definitions.
    System {
        /// The system prompt content
        content: String,
    },

    /// User message containing the input request.
    User {
        /// The user's message text
        content: String,
    },

    /// Assistant message containing either text or tool call requests.
    Assistant {
        /// Text content of the response (may be empty if tool calls are present)
        content: String,

        /// Optional tool calls requested by the assistant
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<ToolCall>>,
    },

    /// Tool execution result message.
    ///
    /// Contains the output from executing a tool call.
    Tool {
        /// ID of the tool call this result corresponds to
        #[serde(rename = "tool_call_id")]
        call_id: String,

        /// The tool's output (success or error message)
        content: String,
    },
}

/// A request from the LLM to execute a tool.
///
/// The LLM generates these when it wants to use a tool instead of
/// responding with text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Unique identifier for this tool call
    pub id: String,

    /// Type of tool call (always "function")
    #[serde(rename = "type")]
    pub call_type: String,

    /// The function to be called
    pub function: ToolFunction,
}

/// Function details within a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunction {
    /// Name of the tool to execute
    pub name: String,

    /// JSON string of arguments to pass to the tool
    pub arguments: String,
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

    /// One or more tool calls to execute
    ToolCalls(Vec<ToolCall>),
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
