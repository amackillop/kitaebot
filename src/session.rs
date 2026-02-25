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
    created_at: Timestamp,
    updated_at: Timestamp,
}

impl Session {
    /// Create a new empty session.
    pub fn new() -> Self {
        let now = Timestamp::now();
        Self {
            messages: Vec::new(),
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
        self.messages.push(message);
        self.updated_at = Timestamp::now();
    }

    /// Load a session from disk, or create a new one if the file doesn't exist.
    pub fn load(path: &Path) -> Result<Self, SessionError> {
        match fs::read_to_string(path) {
            Ok(data) => serde_json::from_str(&data).map_err(SessionError::Parse),
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

    /// Clear conversation history, preserving `created_at`.
    pub fn clear(&mut self) {
        self.messages.clear();
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
}
