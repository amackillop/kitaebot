//! Memory distillation state and gate (spec 21).
//!
//! Distillation folds recent session history into `memory/`. This
//! module owns the persisted per-session watermarks and the mechanical
//! token gate that decides when a pass is worth an LLM turn. The
//! distiller worker and its heartbeat wiring land in later commits.

// Built up across commits; nothing calls the state or gate until the
// heartbeat duty wires distillation (spec 21).
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

/// Persisted distillation progress: the per-session event position
/// already folded into memory. A session absent from the map has never
/// been distilled and counts from its first event.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct DistillState {
    #[serde(default)]
    pub watermarks: BTreeMap<String, u64>,
}

impl DistillState {
    /// Load state from disk. A missing or corrupt file yields empty
    /// watermarks (distill from the start), mirroring the channel poll
    /// cursors.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(contents) => match serde_json::from_str::<Self>(&contents) {
                Ok(state) => state,
                Err(e) => {
                    warn!("Corrupt distillation state, starting empty: {e}");
                    Self::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                info!("No distillation state file, starting empty");
                Self::default()
            }
            Err(e) => {
                warn!("Failed to read distillation state, starting empty: {e}");
                Self::default()
            }
        }
    }

    /// Persist state atomically (tmp + rename).
    pub fn save(&self, path: &Path) {
        let json = match serde_json::to_string(self) {
            Ok(j) => j,
            Err(e) => {
                error!("Failed to serialize distillation state: {e}");
                return;
            }
        };
        let tmp = path.with_extension("tmp");
        if let Err(e) = std::fs::write(&tmp, &json) {
            error!("Failed to write distillation state tmp: {e}");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            error!("Failed to rename distillation state: {e}");
        }
    }
}

/// Total undistilled tokens across all sessions: the value the gate
/// weighs against the threshold. The engine's per-session pending map
/// already subtracts each watermark, so this is a plain sum.
pub fn total_pending(pending: &BTreeMap<String, u64>) -> u64 {
    pending.values().copied().sum()
}

/// The gate opens once the pending total reaches the threshold. A zero
/// threshold is rejected in config, so an empty backlog never fires.
pub fn gate_open(total: u64, threshold: u64) -> bool {
    total >= threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("distillation_state.json");

        let mut state = DistillState::default();
        state.watermarks.insert("general".into(), 12);
        state.watermarks.insert("owner/repo".into(), 3);
        state.save(&path);

        let loaded = DistillState::load(&path);
        assert_eq!(loaded.watermarks.get("general"), Some(&12));
        assert_eq!(loaded.watermarks.get("owner/repo"), Some(&3));
    }

    #[test]
    fn load_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does_not_exist.json");
        assert!(DistillState::load(&path).watermarks.is_empty());
    }

    #[test]
    fn load_corrupt_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("distillation_state.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(DistillState::load(&path).watermarks.is_empty());
    }

    #[test]
    fn total_pending_sums_all_sessions() {
        let pending = BTreeMap::from([("a".into(), 100), ("b".into(), 250)]);
        assert_eq!(total_pending(&pending), 350);
        assert_eq!(total_pending(&BTreeMap::new()), 0);
    }

    #[test]
    fn gate_opens_at_threshold() {
        assert!(!gate_open(999, 1000));
        assert!(gate_open(1000, 1000));
        assert!(gate_open(1001, 1000));
    }
}
