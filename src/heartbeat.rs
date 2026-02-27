//! Periodic heartbeat execution.
//!
//! Reads `HEARTBEAT.md` for active tasks, sends them to the agent for
//! processing, and logs the result to `memory/HISTORY.md`. Skips
//! gracefully when there is nothing to do or another session is active.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::time::SystemTime;

use crate::agent;
use crate::error::{Error, HeartbeatError};
use crate::lock::Lock;
use crate::provider::Provider;
use crate::session::Session;
use crate::tools::Tools;
use crate::workspace::Workspace;

/// Why a heartbeat was skipped (not an error).
#[derive(Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// No `HEARTBEAT.md` file in workspace.
    NoHeartbeatFile,
    /// File exists but contains no unchecked tasks.
    NoActiveTasks,
    /// A user session (REPL or messaging channel) holds the lock.
    SessionActive,
    /// Another heartbeat process is already running.
    HeartbeatLocked,
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoHeartbeatFile => write!(f, "no HEARTBEAT.md"),
            Self::NoActiveTasks => write!(f, "no active tasks"),
            Self::SessionActive => write!(f, "user session active"),
            Self::HeartbeatLocked => write!(f, "heartbeat already running"),
        }
    }
}

/// Result of a heartbeat run.
#[derive(Debug)]
pub enum Outcome {
    /// Agent processed tasks and produced a response.
    Executed(String),
    /// Heartbeat was skipped for a known reason.
    Skipped(SkipReason),
}

/// Run a single heartbeat cycle.
///
/// # Flow
/// 1. Check REPL lock — skip if a user session is active
/// 2. Acquire heartbeat lock — skip if another heartbeat is running
/// 3. Read `HEARTBEAT.md` — skip if missing
/// 4. Parse active tasks — skip if none
/// 5. Build prompt and run one agent turn
/// 6. Append result to `memory/HISTORY.md`
pub async fn run<P: Provider>(
    workspace: &Workspace,
    provider: &P,
    tools: &Tools,
    max_iterations: usize,
) -> Result<Outcome, Error> {
    if Lock::is_held(&workspace.repl_lock_path()) {
        return Ok(Outcome::Skipped(SkipReason::SessionActive));
    }

    let Ok(_lock) = Lock::acquire(&workspace.heartbeat_lock_path()) else {
        return Ok(Outcome::Skipped(SkipReason::HeartbeatLocked));
    };

    let content = match fs::read_to_string(workspace.heartbeat_path()) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Outcome::Skipped(SkipReason::NoHeartbeatFile));
        }
        Err(e) => return Err(HeartbeatError::ReadTasks(e).into()),
    };

    let tasks = parse_active_tasks(&content);
    if tasks.is_empty() {
        return Ok(Outcome::Skipped(SkipReason::NoActiveTasks));
    }

    let prompt = build_prompt(&tasks);
    let system_prompt = workspace.system_prompt();
    let mut session = Session::new();

    let response = agent::run_turn(
        &mut session,
        &system_prompt,
        &prompt,
        provider,
        tools,
        max_iterations,
    )
    .await?;

    append_history(&workspace.history_path(), &response).map_err(HeartbeatError::WriteHistory)?;

    Ok(Outcome::Executed(response))
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
    let timestamp = format_timestamp(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_secs(),
    );

    let mut file = OpenOptions::new().create(true).append(true).open(path)?;

    write!(file, "[{timestamp}] Heartbeat: {response}\n\n")
}

/// Format a Unix epoch as `YYYY-MM-DD HH:MM` UTC.
///
/// Uses Hinnant's `civil_from_days` algorithm to avoid pulling in `chrono`.
fn format_timestamp(epoch: u64) -> String {
    let days_since_epoch = (epoch / 86400).cast_signed();
    let time_of_day = epoch % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;

    let (year, month, day) = civil_from_days(days_since_epoch);

    format!("{year:04}-{month:02}-{day:02} {hours:02}:{minutes:02}")
}

/// Convert days since 1970-01-01 to (year, month, day).
///
/// Howard Hinnant's algorithm. See:
/// <https://howardhinnant.github.io/date_algorithms.html#civil_from_days>
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = u32::try_from(z.rem_euclid(146_097)).expect("day-of-era fits in u32");
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = i64::from(yoe) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::MockProvider;
    use crate::tools::Tools;
    use crate::types::Response;

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
    fn format_timestamp_epoch_zero() {
        assert_eq!(format_timestamp(0), "1970-01-01 00:00");
    }

    #[test]
    fn format_timestamp_y2k() {
        // 2000-01-01 00:00:00 UTC = 946684800
        assert_eq!(format_timestamp(946_684_800), "2000-01-01 00:00");
    }

    #[test]
    fn format_timestamp_with_time() {
        // 2024-02-21 00:00:00 UTC = 1708473600
        assert_eq!(
            format_timestamp(1_708_473_600 + 14 * 3600 + 30 * 60),
            "2024-02-21 14:30"
        );
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

    // -- integration tests for heartbeat::run --

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::init_at(dir.path().to_path_buf()).unwrap();
        (dir, ws)
    }

    #[tokio::test]
    async fn run_skips_when_no_heartbeat_file() {
        let (_dir, ws) = workspace();
        let provider = MockProvider::new(vec![]);
        let outcome = run(&ws, &provider, &Tools::default(), 1).await.unwrap();
        assert!(matches!(
            outcome,
            Outcome::Skipped(SkipReason::NoHeartbeatFile)
        ));
    }

    #[tokio::test]
    async fn run_skips_when_no_active_tasks() {
        let (_dir, ws) = workspace();
        fs::write(ws.heartbeat_path(), "- [x] Done\n- [x] Also done\n").unwrap();

        let provider = MockProvider::new(vec![]);
        let outcome = run(&ws, &provider, &Tools::default(), 1).await.unwrap();
        assert!(matches!(
            outcome,
            Outcome::Skipped(SkipReason::NoActiveTasks)
        ));
    }

    #[tokio::test]
    async fn run_skips_when_repl_lock_held() {
        let (_dir, ws) = workspace();
        fs::write(ws.heartbeat_path(), "- [ ] Pending task\n").unwrap();
        // Write current PID so Lock::is_held returns true.
        fs::write(ws.repl_lock_path(), std::process::id().to_string()).unwrap();

        let provider = MockProvider::new(vec![]);
        let outcome = run(&ws, &provider, &Tools::default(), 1).await.unwrap();
        assert!(matches!(
            outcome,
            Outcome::Skipped(SkipReason::SessionActive)
        ));
    }

    #[tokio::test]
    async fn run_skips_when_heartbeat_lock_held() {
        let (_dir, ws) = workspace();
        fs::write(ws.heartbeat_path(), "- [ ] Pending task\n").unwrap();
        // Hold the heartbeat lock for the duration of this test.
        let _lock = Lock::acquire(&ws.heartbeat_lock_path()).unwrap();

        let provider = MockProvider::new(vec![]);
        let outcome = run(&ws, &provider, &Tools::default(), 1).await.unwrap();
        assert!(matches!(
            outcome,
            Outcome::Skipped(SkipReason::HeartbeatLocked)
        ));
    }

    #[tokio::test]
    async fn run_executes_and_writes_history() {
        let (_dir, ws) = workspace();
        fs::write(ws.heartbeat_path(), "- [ ] Check builds\n").unwrap();

        let provider = MockProvider::new(vec![Ok(Response::Text("All builds green".into()))]);
        let outcome = run(&ws, &provider, &Tools::default(), 1).await.unwrap();

        match outcome {
            Outcome::Executed(ref text) => assert_eq!(text, "All builds green"),
            other @ Outcome::Skipped(_) => panic!("expected Executed, got {other:?}"),
        }

        let history = fs::read_to_string(ws.history_path()).unwrap();
        assert!(history.contains("All builds green"));
        assert!(history.contains("Heartbeat:"));
    }
}
