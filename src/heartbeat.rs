//! Periodic heartbeat channel.
//!
//! [`poll_loop`] ticks on a configurable interval and sends `/heartbeat`
//! through the agent handle. The command handler in [`crate::commands`]
//! does the actual prepare/execute/finish work.
//!
//! [`prepare`] and [`finish`] are the lower-level building blocks used
//! by the `/heartbeat` slash command.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::time::Duration;

use tokio::time::{self, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::agent::AgentHandle;
use crate::agent::envelope::ChannelSource;
use crate::error::HeartbeatError;
use crate::workspace::Workspace;

/// Why a heartbeat was skipped (not an error).
#[derive(Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// File exists but contains no unchecked tasks.
    NoActiveTasks,
    /// No `HEARTBEAT.md` file in workspace.
    NoHeartbeatFile,
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoActiveTasks => write!(f, "no active tasks"),
            Self::NoHeartbeatFile => write!(f, "no HEARTBEAT.md"),
        }
    }
}

/// Result of [`prepare`].
pub enum Prepared {
    /// Prompt built. Send it to the agent.
    Ready(String),
    /// Nothing to do.
    Skipped(SkipReason),
}

/// Run the heartbeat channel loop.
///
/// Sends `/heartbeat` to the agent on each tick. The command handler
/// does prepare/execute/finish; this loop just provides the timer.
pub async fn poll_loop(interval: Duration, handle: &AgentHandle) -> ! {
    let mut tick = time::interval(interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tick.tick().await;
        let cancel = CancellationToken::new();
        match handle
            .send_message(ChannelSource::Heartbeat, "/heartbeat".into(), None, cancel)
            .await
        {
            Ok(reply) => info!("Heartbeat: {}", reply.content),
            Err(e) => error!("Heartbeat error (will retry next tick): {e}"),
        }
    }
}

/// Read tasks and build prompt. Returns [`Prepared`].
pub fn prepare(workspace: &Workspace) -> Result<Prepared, HeartbeatError> {
    let content = match fs::read_to_string(workspace.heartbeat_path()) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Prepared::Skipped(SkipReason::NoHeartbeatFile));
        }
        Err(e) => return Err(HeartbeatError::ReadTasks(e)),
    };

    let tasks = parse_active_tasks(&content);
    if tasks.is_empty() {
        return Ok(Prepared::Skipped(SkipReason::NoActiveTasks));
    }

    Ok(Prepared::Ready(build_prompt(&tasks)))
}

/// Append a timestamped response to `memory/HISTORY.md`.
pub fn finish(workspace: &Workspace, response: &str) -> Result<(), HeartbeatError> {
    append_history(&workspace.history_path(), response).map_err(HeartbeatError::WriteHistory)
}

/// Extract unchecked task lines (`- [ ]`) from markdown content.
fn parse_active_tasks(content: &str) -> Vec<&str> {
    content
        .lines()
        .filter(|line| line.trim_start().starts_with("- [ ]"))
        .collect()
}

/// Build the heartbeat prompt from active tasks.
fn build_prompt(tasks: &[&str]) -> String {
    let mut prompt = String::from(
        "This is a heartbeat check. Review the following tasks and handle any that need attention:\n\n",
    );
    for task in tasks {
        prompt.push_str(task);
        prompt.push('\n');
    }
    prompt
}

/// Append a timestamped entry to the history file.
fn append_history(path: &std::path::Path, response: &str) -> Result<(), std::io::Error> {
    let timestamp = crate::time::now_iso8601();
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    write!(file, "[{timestamp}] Heartbeat: {response}\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_finds_unchecked_tasks() {
        let content = "\
# Heartbeat Tasks

## Active

- [ ] Check builds
- [x] Already done
- [ ] Review memory
";
        let tasks = parse_active_tasks(content);
        assert_eq!(tasks.len(), 2);
        assert!(tasks[0].contains("Check builds"));
        assert!(tasks[1].contains("Review memory"));
    }

    #[test]
    fn parse_handles_indented_tasks() {
        let content = "  - [ ] Indented task\n";
        let tasks = parse_active_tasks(content);
        assert_eq!(tasks.len(), 1);
    }

    #[test]
    fn parse_empty_when_no_tasks() {
        let content = "# Heartbeat\n\nNo tasks here.\n- [x] Done\n";
        let tasks = parse_active_tasks(content);
        assert!(tasks.is_empty());
    }

    #[test]
    fn build_prompt_includes_tasks() {
        let tasks = vec!["- [ ] Check builds", "- [ ] Review memory"];
        let prompt = build_prompt(&tasks);
        assert!(prompt.contains("heartbeat"));
        assert!(prompt.contains("Check builds"));
        assert!(prompt.contains("Review memory"));
    }

    #[test]
    fn append_history_creates_and_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("HISTORY.md");

        append_history(&path, "First entry").unwrap();
        append_history(&path, "Second entry").unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("First entry"));
        assert!(content.contains("Second entry"));
        assert!(content.contains("Heartbeat:"));
        // Two entries, each ending with double newline
        assert_eq!(content.matches("Heartbeat:").count(), 2);
    }

    // -- prepare tests --

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::init_at(dir.path().to_path_buf()).unwrap();
        (dir, ws)
    }

    #[test]
    fn prepare_skips_when_no_heartbeat_file() {
        let (_dir, ws) = workspace();
        let result = prepare(&ws).unwrap();
        assert!(matches!(
            result,
            Prepared::Skipped(SkipReason::NoHeartbeatFile)
        ));
    }

    #[test]
    fn prepare_skips_when_no_active_tasks() {
        let (_dir, ws) = workspace();
        fs::write(ws.heartbeat_path(), "- [x] Done\n- [x] Also done\n").unwrap();
        let result = prepare(&ws).unwrap();
        assert!(matches!(
            result,
            Prepared::Skipped(SkipReason::NoActiveTasks)
        ));
    }

    #[test]
    fn prepare_returns_ready_with_prompt() {
        let (_dir, ws) = workspace();
        fs::write(ws.heartbeat_path(), "- [ ] Check builds\n").unwrap();
        let result = prepare(&ws).unwrap();
        match result {
            Prepared::Ready(prompt) => assert!(prompt.contains("Check builds")),
            Prepared::Skipped(reason) => panic!("expected Ready, got Skipped({reason})"),
        }
    }
}
