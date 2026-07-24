//! Persisted duty state: per-duty last-run epochs.
//!
//! JSON in `state/duties.json`, following the poll-cursor pattern.
//! Loss is benign by design (spec 24): a missing or corrupt file makes
//! every duty due once, via the anacron catch-up.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tracing::{error, warn};

/// Per-duty scheduling state.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct DutyState {
    last_run: HashMap<String, u64>,
}

impl DutyState {
    /// Load from `path`; a missing or corrupt file starts fresh.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_else(|e| {
                warn!("Corrupt duty state, starting fresh: {e}");
                Self::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                warn!("Failed to read duty state, starting fresh: {e}");
                Self::default()
            }
        }
    }

    /// Atomic write: tmp + rename. Failure is logged, not fatal —
    /// worst case is a catch-up run after the next restart.
    pub fn save(&self, path: &Path) {
        let json = match serde_json::to_string(self) {
            Ok(j) => j,
            Err(e) => {
                error!("Failed to serialize duty state: {e}");
                return;
            }
        };
        let tmp = path.with_extension("tmp");
        if let Err(e) = std::fs::write(&tmp, &json) {
            error!("Failed to write duty state tmp: {e}");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            error!("Failed to rename duty state into place: {e}");
        }
    }

    pub fn last_run(&self, duty: &str) -> Option<u64> {
        self.last_run.get(duty).copied()
    }

    pub fn record_run(&mut self, duty: &str, epoch: u64) {
        self.last_run.insert(duty.to_string(), epoch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("duties.json");
        let mut state = DutyState::default();
        state.record_run("heartbeat", 12_345);
        state.save(&path);

        let loaded = DutyState::load(&path);
        assert_eq!(loaded.last_run("heartbeat"), Some(12_345));
        assert_eq!(loaded.last_run("unknown"), None);
    }

    #[test]
    fn missing_file_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let state = DutyState::load(&dir.path().join("duties.json"));
        assert_eq!(state.last_run("heartbeat"), None);
    }

    #[test]
    fn corrupt_file_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("duties.json");
        std::fs::write(&path, "not json").unwrap();
        let state = DutyState::load(&path);
        assert_eq!(state.last_run("heartbeat"), None);
    }
}
