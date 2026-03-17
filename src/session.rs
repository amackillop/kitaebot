//! Session persistence.
//!
//! Stores conversation history as a JSON file in the workspace. Sessions
//! survive restarts so the agent can remember previous interactions.

use std::fs;
use std::path::Path;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::error::SessionError;
use crate::types::Message;

/// A persisted conversation session.
#[derive(Debug, Serialize, Deserialize)]
pub struct Session {
    messages: Vec<Message>,
    #[serde(skip)]
    char_count: usize,
    created_at: Timestamp,
    updated_at: Timestamp,
}

impl Session {
    /// Create a new empty session.
    pub fn new() -> Self {
        let now = Timestamp::now();
        Self {
            messages: Vec::new(),
            char_count: 0,
            created_at: now,
            updated_at: now,
        }
    }

    /// Get the conversation history.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Add a message to the session.
    pub fn add_message(&mut self, message: Message) {
        self.char_count += message.char_count();
        self.messages.push(message);
        self.updated_at = Timestamp::now();
    }

    /// Load a session from disk, or create a new one if the file doesn't exist.
    pub fn load(path: &Path) -> Result<Self, SessionError> {
        match fs::read_to_string(path) {
            Ok(data) => {
                let mut session: Self = serde_json::from_str(&data).map_err(SessionError::Parse)?;
                session.char_count = session.messages.iter().map(Message::char_count).sum();
                Ok(session)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::new()),
            Err(e) => Err(SessionError::Io(e)),
        }
    }

    /// Save the session to disk atomically (write tmp + rename).
    pub fn save(&mut self, path: &Path) -> Result<(), SessionError> {
        self.updated_at = Timestamp::now();

        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_string_pretty(self).map_err(SessionError::Serialize)?;
        fs::write(&tmp, data)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Replace all messages with a single summary message.
    ///
    /// Used by context compaction to shrink conversation history while
    /// preserving a condensed record of what happened.
    pub fn compact(&mut self, summary: Message) {
        self.char_count = summary.char_count();
        self.messages.clear();
        self.messages.push(summary);
        self.updated_at = Timestamp::now();
    }

    /// Number of messages in the session.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Total character count across all messages.
    pub fn char_count(&self) -> usize {
        self.char_count
    }

    /// Clear conversation history, preserving `created_at`.
    pub fn clear(&mut self) {
        self.messages.clear();
        self.char_count = 0;
        self.updated_at = Timestamp::now();
    }
}

/// UTC timestamp serialized as seconds since Unix epoch.
///
/// Avoids pulling in `chrono` for two timestamp fields. The JSON
/// representation is a numeric epoch, which is unambiguous and trivially
/// comparable across systems.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
struct Timestamp(u64);

impl Timestamp {
    fn now() -> Self {
        let secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_secs();
        Self(secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_session_is_empty() {
        let session = Session::new();
        assert!(session.messages.is_empty());
    }

    #[test]
    fn load_missing_file_creates_new() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");

        let session = Session::load(&path).unwrap();
        assert!(session.messages.is_empty());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");

        let mut session = Session::new();
        session.add_message(Message::User {
            content: "hello".to_string(),
        });
        session.save(&path).unwrap();

        let loaded = Session::load(&path).unwrap();
        assert_eq!(loaded.messages.len(), 1);
    }

    #[test]
    fn save_is_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");

        let mut session = Session::new();
        session.save(&path).unwrap();

        assert!(!path.with_extension("json.tmp").exists());
        assert!(path.exists());
    }

    #[test]
    fn clear_preserves_created_at() {
        let mut session = Session::new();
        let created = session.created_at;

        session.add_message(Message::User {
            content: "test".to_string(),
        });
        session.clear();

        assert!(session.messages.is_empty());
        assert_eq!(session.created_at, created);
    }

    #[test]
    fn load_corrupt_file_returns_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");

        fs::write(&path, "not json").unwrap();
        let result = Session::load(&path);
        assert!(matches!(result, Err(SessionError::Parse(_))));
    }

    #[test]
    fn compact_replaces_all_messages_with_summary() {
        let mut session = Session::new();
        for i in 0..5 {
            session.add_message(Message::User {
                content: format!("msg{i}"),
            });
        }

        let summary = Message::System {
            content: "summary of conversation".to_string(),
        };
        session.compact(summary);

        assert_eq!(session.messages.len(), 1);
        assert!(
            matches!(&session.messages[0], Message::System { content } if content == "summary of conversation")
        );
    }

    #[test]
    fn len_returns_message_count() {
        let mut session = Session::new();
        assert_eq!(session.len(), 0);
        session.add_message(Message::User {
            content: "x".to_string(),
        });
        assert_eq!(session.len(), 1);
    }

    #[test]
    fn char_count_tracks_added_messages() {
        let mut session = Session::new();
        assert_eq!(session.char_count(), 0);

        session.add_message(Message::User {
            content: "hello".to_string(),
        });
        assert_eq!(session.char_count(), 5);

        session.add_message(Message::Assistant {
            content: "world".to_string(),
        });
        assert_eq!(session.char_count(), 10);
    }

    #[test]
    fn char_count_includes_all_message_types() {
        let mut session = Session::new();

        session.add_message(Message::System {
            content: "sys".to_string(),
        });
        assert_eq!(session.char_count(), 3);

        session.add_message(Message::User {
            content: "user".to_string(),
        });
        assert_eq!(session.char_count(), 7);

        session.add_message(Message::Assistant {
            content: "assistant".to_string(),
        });
        assert_eq!(session.char_count(), 16);

        session.add_message(Message::Tool {
            call_id: "id".to_string(),
            content: "tool".to_string(),
        });
        assert_eq!(session.char_count(), 20);
    }

    #[test]
    fn char_count_reset_on_clear() {
        let mut session = Session::new();
        session.add_message(Message::User {
            content: "hello world".to_string(),
        });
        assert_eq!(session.char_count(), 11);

        session.clear();
        assert_eq!(session.char_count(), 0);
        assert!(session.messages.is_empty());
    }

    #[test]
    fn char_count_updated_on_compact() {
        let mut session = Session::new();
        session.add_message(Message::User {
            content: "first message".to_string(),
        });
        session.add_message(Message::User {
            content: "second message".to_string(),
        });
        assert_eq!(session.char_count(), 27);

        let summary = Message::System {
            content: "summary".to_string(),
        };
        session.compact(summary);

        assert_eq!(session.char_count(), 7);
        assert_eq!(session.messages.len(), 1);
    }

    #[test]
    fn char_count_recomputed_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");

        let mut session = Session::new();
        session.add_message(Message::User {
            content: "hello".to_string(),
        });
        session.add_message(Message::Assistant {
            content: "world".to_string(),
        });
        session.save(&path).unwrap();

        // char_count is not serialized, so loaded session must recompute it
        let loaded = Session::load(&path).unwrap();
        assert_eq!(loaded.char_count(), 10);
    }

    #[test]
    fn char_count_empty_session() {
        let session = Session::new();
        assert_eq!(session.char_count(), 0);
    }

    #[test]
    fn char_count_with_tool_calls() {
        use crate::types::ToolCall;
        use crate::types::ToolFunction;

        let mut session = Session::new();
        session.add_message(Message::ToolCalls {
            content: "content".to_string(),
            calls: vec![ToolCall::new(
                "id".to_string(),
                ToolFunction {
                    name: "func".to_string(),
                    arguments: "{}".to_string(),
                },
            )],
        });
        // Message content (7) + tool call name (4) + arguments (2) = 13
        assert_eq!(session.char_count(), 13);
    }
}
