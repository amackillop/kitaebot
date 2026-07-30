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

use crate::state_db::StateDb;

use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::agent::AgentHandle;
use crate::agent::envelope::ChannelSource;
use crate::tools::git::GitCli;
use schedule::Schedule;
use state::DutyState;

/// A scheduled duty: performs its [`Action`] when due.
pub struct Duty {
    pub name: String,
    pub action: Action,
    pub schedule: Schedule,
    pub gate: Option<Gate>,
}

/// What a due duty does.
pub enum Action {
    /// Send `input` through the actor: a slash command for built-ins
    /// (`/duty distill`) or the operator's prompt text for prompt
    /// duties; `session_hint` routes onto a repo's work session.
    Dispatch {
        input: String,
        session_hint: Option<String>,
    },
    /// Prepare and warm every repo with a configured warm command, in
    /// the scheduler with no LLM turn (spec 24 self-maintenance).
    Warm,
}

/// A mechanical pre-dispatch check: decides whether a due duty gets a
/// turn at all, at the cost of zero tokens.
pub enum Gate {
    /// Open only when the repo's remote head moved past the cursor.
    NewCommits { repo: String },
}

/// What a new-commits probe decides, given cursor and remote head.
#[derive(Debug, PartialEq, Eq)]
enum NewCommits {
    /// No cursor yet: record the head, dispatch nothing. First contact
    /// must not review the repo's entire history.
    Prime,
    /// Head unchanged: nothing to review.
    Closed,
    /// Head moved: dispatch, and advance the cursor on success.
    Open,
}

fn new_commits_decision(cursor: Option<&str>, head: &str) -> NewCommits {
    match cursor {
        None => NewCommits::Prime,
        Some(c) if c == head => NewCommits::Closed,
        Some(_) => NewCommits::Open,
    }
}

/// Journal a mechanical duty's outcome. Dispatch duties are journaled
/// by the actor like every unattended turn; mechanical duties never
/// pass through it, so the scheduler writes their record itself.
fn record(journal_path: &std::path::Path, name: &str, outcome: &str) {
    if let Err(e) = crate::workspace::journal(journal_path, "duty", &format!("{name}: {outcome}")) {
        warn!(duty = %name, "failed to journal duty outcome: {e}");
    }
}

/// Cap on scheduler sleep. `tokio::time::sleep` is monotonic and
/// diverges from the wall clock across host pauses; re-checking
/// bounds the divergence without ever dispatching early.
const MAX_SLEEP_SECS: u64 = 600;

/// Duties due at `now`, in declaration order.
fn due<'a>(duties: &'a [Duty], state: &DutyState, now: u64) -> Vec<&'a Duty> {
    duties
        .iter()
        .filter(|d| d.schedule.next_due(state.last_run(&d.name), now) <= now)
        .collect()
}

/// Seconds until the earliest next due time, capped at
/// [`MAX_SLEEP_SECS`].
fn next_wake(duties: &[Duty], state: &DutyState, now: u64) -> u64 {
    duties
        .iter()
        .map(|d| {
            d.schedule
                .next_due(state.last_run(&d.name), now)
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
pub async fn run_loop(
    duties: Vec<Duty>,
    state_db: StateDb,
    journal_path: PathBuf,
    handle: &AgentHandle,
    git: Option<GitCli>,
) -> ! {
    let mut state = DutyState::load(&state_db);
    loop {
        let now = crate::time::now_epoch();
        for duty in due(&duties, &state, now) {
            match &duty.action {
                Action::Dispatch {
                    input,
                    session_hint,
                } => {
                    dispatch(
                        duty,
                        input,
                        session_hint.clone(),
                        handle,
                        git.as_ref(),
                        &mut state,
                    )
                    .await;
                }
                Action::Warm => {
                    let outcome = match git.as_ref() {
                        Some(git) => git.warm_configured_repos().await,
                        None => "skipped: no GitCli".into(),
                    };
                    info!(duty = %duty.name, "Duty run: {outcome}");
                    record(&journal_path, &duty.name, &outcome);
                }
            }
            // last_run advances even on error or closed gate: retry
            // next period, not in a tight loop (spec 24 failure modes).
            state.record_run(&duty.name, crate::time::now_epoch());
            state.save(&state_db);
        }
        let now = crate::time::now_epoch();
        tokio::time::sleep(Duration::from_secs(next_wake(&duties, &state, now))).await;
    }
}

/// Gate-check and send one dispatch duty through the actor. The
/// outcome is journaled by the actor (unattended turn), not here.
async fn dispatch(
    duty: &Duty,
    input: &str,
    session_hint: Option<String>,
    handle: &AgentHandle,
    git: Option<&GitCli>,
    state: &mut DutyState,
) {
    // (input, cursor to advance on success)
    let dispatch: Option<(String, Option<String>)> = match &duty.gate {
        None => Some((input.to_string(), None)),
        Some(Gate::NewCommits { repo }) => probe_new_commits(duty, input, repo, git, state).await,
    };
    let Some((input, new_cursor)) = dispatch else {
        return;
    };
    let cancel = CancellationToken::new();
    match handle
        .send_message(ChannelSource::Duty, input, session_hint, None, cancel)
        .await
    {
        Ok(reply) => {
            info!(duty = %duty.name, "Duty run: {}", reply.content);
            // Advance only on success: a failed turn re-reviews the
            // same delta next period.
            if let Some(head) = new_cursor {
                state.set_cursor(&duty.name, &head);
            }
        }
        Err(e) => {
            error!(duty = %duty.name, "Duty error (will retry next period): {e}");
        }
    }
}

/// Probe the new-commits gate. Returns the dispatch input and the
/// cursor to advance, or `None` when nothing should run (gate closed,
/// first-contact priming, or probe failure).
async fn probe_new_commits(
    duty: &Duty,
    input: &str,
    repo: &str,
    git: Option<&GitCli>,
    state: &mut DutyState,
) -> Option<(String, Option<String>)> {
    let Some(git) = git else {
        // Config validation requires github.enabled for gated duties;
        // reaching here means the invariant broke, not the operator.
        error!(duty = %duty.name, "new-commits gate has no GitCli; skipping");
        return None;
    };
    let head = match git.remote_head(repo).await {
        Ok(head) => head,
        Err(e) => {
            error!(duty = %duty.name, "new-commits probe failed (will retry next period): {e}");
            return None;
        }
    };
    match new_commits_decision(state.cursor(&duty.name), &head) {
        NewCommits::Prime => {
            info!(duty = %duty.name, %head, "new-commits gate primed");
            state.set_cursor(&duty.name, &head);
            None
        }
        NewCommits::Closed => {
            info!(duty = %duty.name, "new-commits gate closed");
            None
        }
        NewCommits::Open => {
            let cursor = state.cursor(&duty.name).expect("Open requires a cursor");
            let input = format!("{input}\n\n[new commits: {cursor}..{head}]");
            Some((input, Some(head)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn duty(name: &str, schedule: Schedule) -> Duty {
        Duty {
            name: name.to_string(),
            action: Action::Dispatch {
                input: format!("/{name}"),
                session_hint: None,
            },
            schedule,
            gate: None,
        }
    }

    #[test]
    fn new_commits_decision_matrix() {
        assert_eq!(new_commits_decision(None, "abc"), NewCommits::Prime);
        assert_eq!(new_commits_decision(Some("abc"), "abc"), NewCommits::Closed);
        assert_eq!(new_commits_decision(Some("abc"), "def"), NewCommits::Open);
    }

    fn duties() -> Vec<Duty> {
        vec![
            duty("first", Schedule::Every(3_600)),
            duty("second", Schedule::Daily(6 * 3_600)),
        ]
    }

    #[test]
    fn due_preserves_declaration_order() {
        let state = DutyState::default();
        // No recorded runs: everything is due, in declaration order.
        let duties = duties();
        let names: Vec<&str> = due(&duties, &state, 1_000)
            .iter()
            .map(|d| d.name.as_str())
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
        let short = vec![duty("soon", Schedule::Every(30))];
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
