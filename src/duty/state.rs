//! Persisted duty state: per-duty last-run epochs and gate cursors.
//!
//! Stored as the `duties` document in the state database (spec 24).
//! Loss is benign by design: a missing or corrupt document makes
//! every duty due once, via the anacron catch-up.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tracing::{error, warn};

use crate::state_db::StateDb;

const DOC: &str = "duties";

/// Per-duty scheduling state.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct DutyState {
    last_run: HashMap<String, u64>,
    /// Per-duty gate cursors (new-commits: last-reviewed head SHA).
    #[serde(default)]
    cursors: HashMap<String, String>,
}

impl DutyState {
    /// Load from the state database; a missing or corrupt document
    /// starts fresh.
    pub fn load(db: &StateDb) -> Self {
        match db.get_doc(DOC) {
            Ok(Some(json)) => serde_json::from_str(&json).unwrap_or_else(|e| {
                warn!("Corrupt duty state, starting fresh: {e}");
                Self::default()
            }),
            Ok(None) => Self::default(),
            Err(e) => {
                warn!("Failed to read duty state, starting fresh: {e}");
                Self::default()
            }
        }
    }

    /// Persist. Failure is logged, not fatal — worst case is a
    /// catch-up run after the next restart.
    pub fn save(&self, db: &StateDb) {
        let json = match serde_json::to_string(self) {
            Ok(j) => j,
            Err(e) => {
                error!("Failed to serialize duty state: {e}");
                return;
            }
        };
        if let Err(e) = db.put_doc(DOC, &json) {
            error!("Failed to write duty state: {e}");
        }
    }

    pub fn last_run(&self, duty: &str) -> Option<u64> {
        self.last_run.get(duty).copied()
    }

    pub fn record_run(&mut self, duty: &str, epoch: u64) {
        self.last_run.insert(duty.to_string(), epoch);
    }

    pub fn cursor(&self, duty: &str) -> Option<&str> {
        self.cursors.get(duty).map(String::as_str)
    }

    pub fn set_cursor(&mut self, duty: &str, value: &str) {
        self.cursors.insert(duty.to_string(), value.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let db = StateDb::open_in_memory().unwrap();
        let mut state = DutyState::default();
        state.record_run("distill", 12_345);
        state.set_cursor("watch", "abc123");
        state.save(&db);

        let loaded = DutyState::load(&db);
        assert_eq!(loaded.last_run("distill"), Some(12_345));
        assert_eq!(loaded.last_run("unknown"), None);
        assert_eq!(loaded.cursor("watch"), Some("abc123"));
        assert_eq!(loaded.cursor("unknown"), None);
    }

    #[test]
    fn cursorless_document_loads() {
        // A duties document written before cursors existed.
        let db = StateDb::open_in_memory().unwrap();
        db.put_doc("duties", r#"{"last_run":{"distill":5}}"#)
            .unwrap();
        let state = DutyState::load(&db);
        assert_eq!(state.last_run("distill"), Some(5));
        assert_eq!(state.cursor("distill"), None);
    }

    #[test]
    fn missing_document_starts_fresh() {
        let db = StateDb::open_in_memory().unwrap();
        let state = DutyState::load(&db);
        assert_eq!(state.last_run("distill"), None);
    }

    #[test]
    fn corrupt_document_starts_fresh() {
        let db = StateDb::open_in_memory().unwrap();
        db.put_doc("duties", "not json").unwrap();
        let state = DutyState::load(&db);
        assert_eq!(state.last_run("distill"), None);
    }
}
