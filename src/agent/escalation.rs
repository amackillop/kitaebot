//! Durable execution-model escalation state.

use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension};

use crate::usage::TaskKey;

/// The provider tier selected for a root turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderTier {
    Default,
    Retry,
}

/// The state transition caused by a completed root turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transition {
    Armed,
    Cleared,
    Exhausted,
}

/// Persistent retry state keyed by the dispatch identity (spec 27).
#[derive(Clone)]
pub struct ExecutionEscalations {
    conn: Arc<Mutex<Connection>>,
}

impl ExecutionEscalations {
    pub fn new(db: &crate::state_db::StateDb) -> Self {
        Self {
            conn: db.connection(),
        }
    }

    /// Select the configured retry provider only after one capped turn.
    pub fn tier(&self, task: &TaskKey) -> rusqlite::Result<ProviderTier> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        conn.query_row(
            "SELECT 1 FROM execution_escalations WHERE task = ?1",
            [task.as_str()],
            |_| Ok(()),
        )
        .optional()
        .map(|row| match row {
            Some(()) => ProviderTier::Retry,
            None => ProviderTier::Default,
        })
    }

    /// Arm a retry after the first cap; clear it after every other result.
    pub fn record(
        &self,
        task: &TaskKey,
        tier: ProviderTier,
        capped: bool,
    ) -> rusqlite::Result<Transition> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match (tier, capped) {
            (ProviderTier::Default, true) => {
                conn.execute(
                    "INSERT INTO execution_escalations (task) VALUES (?1)
                     ON CONFLICT(task) DO NOTHING",
                    [task.as_str()],
                )?;
                Ok(Transition::Armed)
            }
            (ProviderTier::Retry, true) => {
                conn.execute(
                    "DELETE FROM execution_escalations WHERE task = ?1",
                    [task.as_str()],
                )?;
                Ok(Transition::Exhausted)
            }
            (_, false) => {
                conn.execute(
                    "DELETE FROM execution_escalations WHERE task = ?1",
                    [task.as_str()],
                )?;
                Ok(Transition::Cleared)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task() -> TaskKey {
        TaskKey::for_source(&crate::agent::envelope::ChannelSource::GitHubIssue {
            issue: "owner/repo#146".into(),
        })
    }

    #[test]
    fn first_cap_arms_retry_and_second_exhausts_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kitaebot.db");
        let db = crate::state_db::StateDb::open(&path).unwrap();
        let escalations = ExecutionEscalations::new(&db);
        let task = task();

        assert_eq!(escalations.tier(&task).unwrap(), ProviderTier::Default);
        assert_eq!(
            escalations
                .record(&task, ProviderTier::Default, true)
                .unwrap(),
            Transition::Armed
        );
        drop(escalations);
        drop(db);

        let db = crate::state_db::StateDb::open(&path).unwrap();
        let escalations = ExecutionEscalations::new(&db);
        assert_eq!(escalations.tier(&task).unwrap(), ProviderTier::Retry);
        assert_eq!(
            escalations
                .record(&task, ProviderTier::Retry, true)
                .unwrap(),
            Transition::Exhausted
        );
        assert_eq!(escalations.tier(&task).unwrap(), ProviderTier::Default);
    }

    #[test]
    fn non_cap_outcome_clears_an_armed_retry() {
        let db = crate::state_db::StateDb::open_in_memory().unwrap();
        let escalations = ExecutionEscalations::new(&db);
        let task = task();
        escalations
            .record(&task, ProviderTier::Default, true)
            .unwrap();

        assert_eq!(
            escalations
                .record(&task, ProviderTier::Retry, false)
                .unwrap(),
            Transition::Cleared
        );
        assert_eq!(escalations.tier(&task).unwrap(), ProviderTier::Default);
    }
}
