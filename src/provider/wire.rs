//! `OpenAI` Chat Completions request wire format.
//!
//! These types exist solely for serialization to the API. They are never
//! deserialized — response wire types live in [`crate::clients::chat_completion`].

use serde::Serialize;

use crate::types::{Message, ToolCall, ToolFunction};

/// Wire-format message for the `OpenAI` Chat Completions request body.
#[derive(Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum WireMessage {
    Assistant {
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<WireToolCall>>,
    },
    System {
        content: String,
    },
    Tool {
        #[serde(rename = "tool_call_id")]
        call_id: String,
        content: String,
    },
    User {
        content: String,
    },
}

/// Wire-format tool call within an assistant message.
#[derive(Serialize)]
pub struct WireToolCall {
    pub id: String,
    pub function: WireFunction,
    #[serde(rename = "type")]
    pub call_type: &'static str,
}

/// Wire-format function within a tool call.
#[derive(Serialize)]
pub struct WireFunction {
    pub name: String,
    pub arguments: String,
}

// ── From conversions (domain → wire) ────────────────────────────────

impl From<&Message> for WireMessage {
    fn from(msg: &Message) -> Self {
        match msg {
            Message::Assistant {
                content,
                tool_calls,
            } => {
                let calls = if tool_calls.is_empty() {
                    None
                } else {
                    Some(tool_calls.iter().map(WireToolCall::from).collect())
                };
                Self::Assistant {
                    content: content.clone(),
                    tool_calls: calls,
                }
            }
            Message::System { content } => Self::System {
                content: content.clone(),
            },
            Message::Tool { call_id, content } => Self::Tool {
                call_id: call_id.clone(),
                content: content.clone(),
            },
            Message::User { content } => Self::User {
                content: content.clone(),
            },
        }
    }
}

impl From<&ToolCall> for WireToolCall {
    fn from(tc: &ToolCall) -> Self {
        Self {
            id: tc.id.clone(),
            function: WireFunction::from(&tc.function),
            call_type: "function",
        }
    }
}

impl From<&ToolFunction> for WireFunction {
    fn from(f: &ToolFunction) -> Self {
        Self {
            name: f.name.clone(),
            arguments: f.arguments.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ToolCall, ToolFunction};

    #[test]
    fn user_message_wire_format() {
        let msg = Message::User {
            content: "hello".to_string(),
        };
        let wire = WireMessage::from(&msg);
        let json = serde_json::to_value(&wire).unwrap();
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"], "hello");
    }

    #[test]
    fn system_message_wire_format() {
        let msg = Message::System {
            content: "be helpful".to_string(),
        };
        let wire = WireMessage::from(&msg);
        let json = serde_json::to_value(&wire).unwrap();
        assert_eq!(json["role"], "system");
        assert_eq!(json["content"], "be helpful");
    }

    #[test]
    fn tool_message_wire_format() {
        let msg = Message::Tool {
            call_id: "call_123".to_string(),
            content: "result".to_string(),
        };
        let wire = WireMessage::from(&msg);
        let json = serde_json::to_value(&wire).unwrap();
        assert_eq!(json["role"], "tool");
        assert_eq!(json["tool_call_id"], "call_123");
        assert_eq!(json["content"], "result");
        // call_id should NOT appear in wire format
        assert!(json.get("call_id").is_none());
    }

    #[test]
    fn assistant_text_only_omits_tool_calls() {
        let msg = Message::Assistant {
            content: "sure".to_string(),
            tool_calls: vec![],
        };
        let wire = WireMessage::from(&msg);
        let json = serde_json::to_value(&wire).unwrap();
        assert_eq!(json["role"], "assistant");
        assert_eq!(json["content"], "sure");
        assert!(json.get("tool_calls").is_none());
    }

    #[test]
    fn assistant_with_tool_calls() {
        let msg = Message::Assistant {
            content: String::new(),
            tool_calls: vec![ToolCall::new(
                "call_1".to_string(),
                ToolFunction {
                    name: "exec".to_string(),
                    arguments: r#"{"command":"ls"}"#.to_string(),
                },
            )],
        };
        let wire = WireMessage::from(&msg);
        let json = serde_json::to_value(&wire).unwrap();
        assert_eq!(json["role"], "assistant");
        let calls = json["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "call_1");
        assert_eq!(calls[0]["type"], "function");
        assert_eq!(calls[0]["function"]["name"], "exec");
    }
}
