//! The duty scheduler (spec 24, phase 1).
//!
//! A duty is a named unit of scheduled work dispatched through the
//! agent actor as a slash command. Schedules are wall-clock (epoch)
//! based with per-duty `last_run` persisted in `state/duties.json`,
//! so cadence survives restarts, and an overdue duty fires once at
//! startup (anacron catch-up) instead of resetting phase or bursting.

pub mod schedule;
pub mod state;

use std::path::PathBuf;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::agent::AgentHandle;
use crate::agent::envelope::ChannelSource;
use schedule::Schedule;
use state::DutyState;

/// A scheduled duty: dispatches `command` through the actor when due.
pub struct Duty {
    pub name: &'static str,
    pub command: &'static str,
    pub schedule: Schedule,
}

/// Cap on scheduler sleep. `tokio::time::sleep` is monotonic and
/// diverges from the wall clock across host pauses; re-checking
/// bounds the divergence without ever dispatching early.
const MAX_SLEEP_SECS: u64 = 600;

/// Duties due at `now`, in declaration order.
fn due<'a>(duties: &'a [Duty], state: &DutyState, now: u64) -> Vec<&'a Duty> {
    duties
        .iter()
        .filter(|d| d.schedule.next_due(state.last_run(d.name), now) <= now)
        .collect()
}

/// Seconds until the earliest next due time, capped at
/// [`MAX_SLEEP_SECS`].
fn next_wake(duties: &[Duty], state: &DutyState, now: u64) -> u64 {
    duties
        .iter()
        .map(|d| {
            d.schedule
                .next_due(state.last_run(d.name), now)
                .saturating_sub(now)
        })
        .min()
        .unwrap_or(MAX_SLEEP_SECS)
        .clamp(1, MAX_SLEEP_SECS)
}

/// Run the duty scheduler loop.
///
/// Serialization comes free: `send_message` awaits the actor, so two
/// duties due together run in sequence, in declaration order.
pub async fn run_loop(duties: Vec<Duty>, state_path: PathBuf, handle: &AgentHandle) -> ! {
    let mut state = DutyState::load(&state_path);
    loop {
        let now = crate::time::now_epoch();
        for duty in due(&duties, &state, now) {
            let cancel = CancellationToken::new();
            match handle
                .send_message(
                    ChannelSource::Duty,
                    duty.command.to_string(),
                    None,
                    None,
                    cancel,
                )
                .await
            {
                Ok(reply) => info!(duty = duty.name, "Duty run: {}", reply.content),
                Err(e) => error!(duty = duty.name, "Duty error (will retry next period): {e}"),
            }
            // last_run advances even on error: retry next period, not
            // in a tight loop (spec 24 failure modes).
            state.record_run(duty.name, crate::time::now_epoch());
            state.save(&state_path);
        }
        let now = crate::time::now_epoch();
        tokio::time::sleep(Duration::from_secs(next_wake(&duties, &state, now))).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn duties() -> Vec<Duty> {
        vec![
            Duty {
                name: "first",
                command: "/first",
                schedule: Schedule::Every(3_600),
            },
            Duty {
                name: "second",
                command: "/second",
                schedule: Schedule::Daily(6 * 3_600),
            },
        ]
    }

    #[test]
    fn due_preserves_declaration_order() {
        let state = DutyState::default();
        // No recorded runs: everything is due, in declaration order.
        let names: Vec<&str> = due(&duties(), &state, 1_000)
            .iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names, ["first", "second"]);
    }

    #[test]
    fn ran_duties_are_not_due() {
        let mut state = DutyState::default();
        let now = 100_000;
        state.record_run("first", now);
        state.record_run("second", now);
        assert!(due(&duties(), &state, now).is_empty());
    }

    #[test]
    fn next_wake_targets_earliest_duty() {
        let mut state = DutyState::default();
        let now = 100_000;
        state.record_run("first", now); // due in 3600
        state.record_run("second", now); // due later
        assert_eq!(next_wake(&duties(), &state, now), 600); // capped
        let short = vec![Duty {
            name: "soon",
            command: "/soon",
            schedule: Schedule::Every(30),
        }];
        state.record_run("soon", now);
        assert_eq!(next_wake(&short, &state, now), 30);
    }

    #[test]
    fn next_wake_never_busy_loops() {
        let state = DutyState::default();
        // Everything due now: still sleeps at least a second.
        assert_eq!(next_wake(&duties(), &state, 1_000), 1);
    }
}
