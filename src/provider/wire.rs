//! `OpenAI` Chat Completions request wire format.
//!
//! These types exist solely for serialization to the API. They are never
//! deserialized — response wire types live in [`crate::clients::chat_completion`].

use serde::Serialize;

use crate::types::{Message, ToolCall, ToolFunction};

/// Wire-format message for the `OpenAI` Chat Completions request body.
#[derive(Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum WireMessage<'a> {
    Assistant {
        content: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<WireToolCall<'a>>>,
    },
    System {
        content: &'a str,
    },
    Tool {
        #[serde(rename = "tool_call_id")]
        call_id: &'a str,
        content: &'a str,
    },
    User {
        content: &'a str,
    },
}

/// Wire-format tool call within an assistant message.
#[derive(Serialize)]
pub struct WireToolCall<'a> {
    pub id: &'a str,
    pub function: WireFunction<'a>,
    #[serde(rename = "type")]
    pub call_type: &'static str,
}

/// Wire-format function within a tool call.
#[derive(Serialize)]
pub struct WireFunction<'a> {
    pub name: &'a str,
    pub arguments: &'a str,
}

// ── From conversions (domain → wire) ────────────────────────────────

impl<'a> From<&'a Message> for WireMessage<'a> {
    fn from(msg: &'a Message) -> Self {
        match msg {
            Message::Assistant { content } => Self::Assistant {
                content,
                tool_calls: None,
            },
            Message::ToolCalls { content, calls } => Self::Assistant {
                content,
                tool_calls: Some(calls.iter().map(WireToolCall::from).collect()),
            },
            Message::System { content } => Self::System { content },
            Message::Tool { call_id, content } => Self::Tool { call_id, content },
            Message::User { content } => Self::User { content },
        }
    }
}

impl<'a> From<&'a ToolCall> for WireToolCall<'a> {
    fn from(tc: &'a ToolCall) -> Self {
        Self {
            id: &tc.id,
            function: WireFunction::from(&tc.function),
            call_type: "function",
        }
    }
}

impl<'a> From<&'a ToolFunction> for WireFunction<'a> {
    fn from(f: &'a ToolFunction) -> Self {
        Self {
            name: &f.name,
            arguments: &f.arguments,
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
        };
        let wire = WireMessage::from(&msg);
        let json = serde_json::to_value(&wire).unwrap();
        assert_eq!(json["role"], "assistant");
        assert_eq!(json["content"], "sure");
        assert!(json.get("tool_calls").is_none());
    }

    #[test]
    fn assistant_with_tool_calls() {
        let msg = Message::ToolCalls {
            content: String::new(),
            calls: vec![ToolCall::new(
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
