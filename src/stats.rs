//! Session statistics.
//!
//! Scans session files and reports which tools and exec commands consume
//! the most output bytes, guiding optimization work.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::path::Path;

use crate::session::Session;
use crate::types::Message;

use std::fmt;

/// Maximum display length for blocked command strings.
const MAX_CMD_DISPLAY: usize = 60;

// ── Domain types ────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct ToolStats {
    calls: u64,
    total_output_bytes: u64,
}

/// How a tool call failed, derived from the Tool message content prefix.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum FailureKind {
    Blocked,
    ExecutionFailed,
    InvalidArguments,
    NotFound,
    Timeout,
    SafetyBlock,
    RepeatBlock,
}

impl fmt::Display for FailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Blocked => write!(f, "Blocked"),
            Self::ExecutionFailed => write!(f, "ExecutionFailed"),
            Self::InvalidArguments => write!(f, "InvalidArguments"),
            Self::NotFound => write!(f, "NotFound"),
            Self::Timeout => write!(f, "Timeout"),
            Self::SafetyBlock => write!(f, "SafetyBlock"),
            Self::RepeatBlock => write!(f, "RepeatBlock"),
        }
    }
}

/// Key for the tool errors table: tool name + failure classification.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ToolErrorKey {
    tool: String,
    kind: FailureKind,
}

#[derive(Debug)]
struct Report {
    session_count: usize,
    by_tool: Vec<(String, ToolStats)>,
    by_exec_cmd: Vec<(String, ToolStats)>,
    /// Exec commands blocked by deny rules, keyed on full command (truncated).
    blocked_cmds: Vec<(String, u64)>,
    /// Non-exec failures and non-Blocked exec failures.
    tool_errors: Vec<(ToolErrorKey, u64)>,
}

/// Extracted context from a tool call needed for attribution.
struct CallInfo {
    tool_name: String,
    /// Two-token command key for the Exec Breakdown table.
    exec_cmd: Option<String>,
    /// Full command string for blocked command display.
    exec_full_cmd: Option<String>,
}

// ── Public entry point ──────────────────────────────────────────────

pub fn run(workspace: &Path) -> String {
    let pattern = workspace.join("sessions/*.json");
    let pattern = pattern.to_string_lossy();

    let (sessions, errors): (Vec<_>, Vec<_>) = glob::glob(&pattern)
        .expect("valid glob pattern")
        .filter_map(|entry| entry.inspect_err(|e| eprintln!("glob error: {e}")).ok())
        .map(|path| Session::load(&path).map_err(|e| (path, e)))
        .partition(Result::is_ok);

    for (path, err) in errors.into_iter().map(Result::unwrap_err) {
        eprintln!("warning: skipping {}: {err}", path.display());
    }

    let sessions: Vec<_> = sessions.into_iter().map(Result::unwrap).collect();
    let report = analyze(&sessions);
    format_report(&report)
}

// ── Analysis (pure, testable) ───────────────────────────────────────
//
// The OpenAI wire format stores tool interactions as separate messages:
//
//   Assistant { tool_calls: [call_0, call_1, ...] }
//   Tool { content: "output_0" }
//   Tool { content: "output_1" }
//
// The agent loop (agent.rs) appends Tool messages in the same order as
// the tool_calls array, so we correlate them positionally: on each
// Assistant message, push call info into a queue; on each Tool message,
// pop from the front. No call_id lookup needed.

/// Accumulator threaded through the message fold.
type Acc = (
    VecDeque<CallInfo>,
    HashMap<String, ToolStats>,
    HashMap<String, ToolStats>,
    HashMap<String, u64>,
    HashMap<ToolErrorKey, u64>,
);

fn analyze(sessions: &[Session]) -> Report {
    let (_, by_tool, by_exec_cmd, blocked_cmds, tool_errors) =
        sessions.iter().flat_map(Session::messages).fold(
            <Acc>::default(),
            |(mut pending, mut by_tool, mut by_exec_cmd, mut blocked_cmds, mut tool_errors),
             msg| {
                match msg {
                    Message::ToolCalls { calls, .. } => {
                        let is_exec = |name: &str| name == "exec";
                        let call_infos = calls.iter().map(|call| CallInfo {
                            tool_name: call.function.name.clone(),
                            exec_cmd: is_exec(&call.function.name)
                                .then(|| extract_exec_command(&call.function.arguments)),
                            exec_full_cmd: is_exec(&call.function.name)
                                .then(|| extract_exec_full_command(&call.function.arguments)),
                        });
                        pending.extend(call_infos);
                    }
                    Message::Tool { content, .. } => {
                        if let Some(info) = pending.pop_front() {
                            let bytes = content.len() as u64;
                            accumulate(&mut by_tool, info.tool_name.clone(), bytes);
                            if let Some(ref cmd) = info.exec_cmd {
                                accumulate(&mut by_exec_cmd, cmd.clone(), bytes);
                            }

                            if let Some(kind) = classify_failure(content) {
                                if kind == FailureKind::Blocked {
                                    if let Some(full_cmd) = info.exec_full_cmd {
                                        *blocked_cmds.entry(full_cmd).or_default() += 1;
                                    } else {
                                        let key = ToolErrorKey {
                                            tool: info.tool_name,
                                            kind,
                                        };
                                        *tool_errors.entry(key).or_default() += 1;
                                    }
                                } else {
                                    let key = ToolErrorKey {
                                        tool: info.tool_name,
                                        kind,
                                    };
                                    *tool_errors.entry(key).or_default() += 1;
                                }
                            }
                        }
                    }
                    _ => {}
                }
                (pending, by_tool, by_exec_cmd, blocked_cmds, tool_errors)
            },
        );

    let sorted_stats = |map: HashMap<String, ToolStats>| {
        let mut v: Vec<_> = map.into_iter().collect();
        v.sort_by(|a, b| b.1.total_output_bytes.cmp(&a.1.total_output_bytes));
        v
    };

    let sorted_counts = |map: HashMap<String, u64>| {
        let mut v: Vec<_> = map.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v
    };

    let sorted_errors = |map: HashMap<ToolErrorKey, u64>| {
        let mut v: Vec<_> = map.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v
    };

    Report {
        session_count: sessions.len(),
        by_tool: sorted_stats(by_tool),
        by_exec_cmd: sorted_stats(by_exec_cmd),
        blocked_cmds: sorted_counts(blocked_cmds),
        tool_errors: sorted_errors(tool_errors),
    }
}

fn accumulate(map: &mut HashMap<String, ToolStats>, key: String, bytes: u64) {
    let entry = map.entry(key).or_default();
    entry.calls += 1;
    entry.total_output_bytes += bytes;
}

// ── Failure classification ───────────────────────────────────────────

/// Classify a tool message as success or failure from its content.
///
/// Returns `None` for successful calls. The content prefixes are produced
/// by `record_tool_results` in `agent.rs` and are stable within the crate.
fn classify_failure(content: &str) -> Option<FailureKind> {
    if content.starts_with("Error: Tool blocked: ") {
        Some(FailureKind::Blocked)
    } else if content.starts_with("Error: Execution failed: ") {
        Some(FailureKind::ExecutionFailed)
    } else if content.starts_with("Error: Invalid arguments: ") {
        Some(FailureKind::InvalidArguments)
    } else if content.starts_with("Error: Tool not found: ") {
        Some(FailureKind::NotFound)
    } else if content == "Error: Tool execution timed out" {
        Some(FailureKind::Timeout)
    } else if content.starts_with("Tool output blocked: ") {
        Some(FailureKind::SafetyBlock)
    } else if content.starts_with("ERROR: You have called this tool") {
        Some(FailureKind::RepeatBlock)
    } else {
        None
    }
}

// ── Exec command extraction ─────────────────────────────────────────

/// Extract a short command key from the exec tool's JSON arguments.
///
/// Takes the `"command"` field and returns the first two whitespace-delimited
/// tokens (e.g. `"git status"`, `"cargo test"`). Single-token commands like
/// `"ls"` remain as-is.
fn extract_exec_command(arguments_json: &str) -> String {
    serde_json::from_str::<serde_json::Value>(arguments_json)
        .ok()
        .and_then(|v| v.get("command")?.as_str().map(String::from))
        .map_or_else(
            || "<unknown>".to_string(),
            |cmd| {
                let key: Vec<&str> = cmd.split_whitespace().take(2).collect();
                if key.is_empty() {
                    "<empty>".to_string()
                } else {
                    key.join(" ")
                }
            },
        )
}

/// Extract the full command string from exec tool JSON arguments.
///
/// Truncated to [`MAX_CMD_DISPLAY`] characters for table display.
fn extract_exec_full_command(arguments_json: &str) -> String {
    let cmd = serde_json::from_str::<serde_json::Value>(arguments_json)
        .ok()
        .and_then(|v| v.get("command")?.as_str().map(String::from))
        .unwrap_or_else(|| "<unknown>".to_string());
    truncate_str(&cmd, MAX_CMD_DISPLAY)
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max - 3])
    }
}

// ── Formatting ──────────────────────────────────────────────────────

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;

    #[allow(clippy::cast_precision_loss)] // precision loss irrelevant for display
    let b = bytes as f64;
    if b < KIB {
        format!("{bytes} B")
    } else if b < MIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{:.1} MiB", b / MIB)
    }
}

fn format_table(out: &mut String, rows: &[(String, ToolStats)]) {
    for (name, stats) in rows {
        let avg = if stats.calls > 0 {
            stats.total_output_bytes / stats.calls
        } else {
            0
        };
        writeln!(
            out,
            "{:<20} {:>6}   {:>14}   {:>10}",
            name,
            stats.calls,
            format_bytes(stats.total_output_bytes),
            format_bytes(avg),
        )
        .unwrap();
    }
}

fn format_report(report: &Report) -> String {
    let mut out = String::new();
    let sessions = report.session_count;

    writeln!(
        out,
        "Tool Usage ({sessions} session{})\n",
        if sessions == 1 { "" } else { "s" }
    )
    .unwrap();

    writeln!(
        out,
        "{:<20} {:>6}   {:>14}   {:>10}",
        "Tool", "Calls", "Total Output", "Avg Output"
    )
    .unwrap();
    writeln!(
        out,
        "{:<20} {:>6}   {:>14}   {:>10}",
        "----", "-----", "------------", "----------"
    )
    .unwrap();
    format_table(&mut out, &report.by_tool);

    if !report.by_exec_cmd.is_empty() {
        writeln!(out, "\nExec Breakdown\n").unwrap();
        writeln!(
            out,
            "{:<20} {:>6}   {:>14}   {:>10}",
            "Command", "Calls", "Total Output", "Avg Output"
        )
        .unwrap();
        writeln!(
            out,
            "{:<20} {:>6}   {:>14}   {:>10}",
            "-------", "-----", "------------", "----------"
        )
        .unwrap();
        format_table(&mut out, &report.by_exec_cmd);
    }

    if !report.blocked_cmds.is_empty() {
        writeln!(out, "\nBlocked Commands\n").unwrap();
        writeln!(out, "{:<60} {:>6}", "Command", "Count").unwrap();
        writeln!(out, "{:<60} {:>6}", "-------", "-----").unwrap();
        for (cmd, count) in &report.blocked_cmds {
            writeln!(out, "{cmd:<60} {count:>6}").unwrap();
        }
    }

    if !report.tool_errors.is_empty() {
        writeln!(out, "\nTool Errors\n").unwrap();
        writeln!(out, "{:<20} {:<20} {:>6}", "Tool", "Error", "Count").unwrap();
        writeln!(out, "{:<20} {:<20} {:>6}", "----", "-----", "-----").unwrap();
        for (key, count) in &report.tool_errors {
            writeln!(out, "{:<20} {:<20} {:>6}", key.tool, key.kind, count).unwrap();
        }
    }

    out
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ToolCall, ToolFunction};

    fn make_assistant(calls: Vec<ToolCall>) -> Message {
        Message::ToolCalls {
            content: String::new(),
            calls,
        }
    }

    fn make_tool(call_id: &str, content: &str) -> Message {
        Message::Tool {
            call_id: call_id.to_string(),
            content: content.to_string(),
        }
    }

    fn make_call(id: &str, name: &str, arguments: &str) -> ToolCall {
        ToolCall::new(
            id.to_string(),
            ToolFunction {
                name: name.to_string(),
                arguments: arguments.to_string(),
            },
        )
    }

    // ── extract_exec_command ────────────────────────────────────────

    #[test]
    fn extract_two_tokens() {
        assert_eq!(
            extract_exec_command(r#"{"command":"git status"}"#),
            "git status"
        );
    }

    #[test]
    fn extract_truncates_to_two_tokens() {
        assert_eq!(
            extract_exec_command(r#"{"command":"git log --oneline -5"}"#),
            "git log"
        );
    }

    #[test]
    fn extract_single_token() {
        assert_eq!(extract_exec_command(r#"{"command":"ls -la"}"#), "ls -la");
        // "ls" alone:
        assert_eq!(extract_exec_command(r#"{"command":"ls"}"#), "ls");
    }

    #[test]
    fn extract_empty_command() {
        assert_eq!(extract_exec_command(r#"{"command":""}"#), "<empty>");
    }

    #[test]
    fn extract_bad_json() {
        assert_eq!(extract_exec_command("not json"), "<unknown>");
    }

    #[test]
    fn extract_missing_command_field() {
        assert_eq!(extract_exec_command(r#"{"other":"val"}"#), "<unknown>");
    }

    // ── format_bytes ────────────────────────────────────────────────

    #[test]
    fn format_bytes_zero() {
        assert_eq!(format_bytes(0), "0 B");
    }

    #[test]
    fn format_bytes_below_kib() {
        assert_eq!(format_bytes(1023), "1023 B");
    }

    #[test]
    fn format_bytes_exact_kib() {
        assert_eq!(format_bytes(1024), "1.0 KiB");
    }

    #[test]
    fn format_bytes_exact_mib() {
        assert_eq!(format_bytes(1_048_576), "1.0 MiB");
    }

    #[test]
    fn format_bytes_fractional_kib() {
        assert_eq!(format_bytes(2560), "2.5 KiB");
    }

    // ── analyze ─────────────────────────────────────────────────────

    fn build_test_session(messages: Vec<Message>) -> Session {
        let mut session = Session::new();
        for msg in messages {
            session.add_message(msg);
        }
        session
    }

    #[test]
    fn analyze_empty_sessions() {
        let report = analyze(&[]);
        assert_eq!(report.session_count, 0);
        assert!(report.by_tool.is_empty());
        assert!(report.by_exec_cmd.is_empty());
    }

    #[test]
    fn analyze_counts_tool_calls() {
        let session = build_test_session(vec![
            make_assistant(vec![make_call("c1", "exec", r#"{"command":"git status"}"#)]),
            make_tool("c1", "on branch main\nnothing to commit"),
            make_assistant(vec![make_call("c2", "file_read", r#"{"path":"foo.rs"}"#)]),
            make_tool("c2", "fn main() {}"),
        ]);

        let report = analyze(&[session]);

        assert_eq!(report.session_count, 1);

        let exec_stats = report.by_tool.iter().find(|(n, _)| n == "exec").unwrap();
        assert_eq!(exec_stats.1.calls, 1);
        assert_eq!(
            exec_stats.1.total_output_bytes,
            "on branch main\nnothing to commit".len() as u64
        );

        let fr_stats = report
            .by_tool
            .iter()
            .find(|(n, _)| n == "file_read")
            .unwrap();
        assert_eq!(fr_stats.1.calls, 1);
        assert_eq!(fr_stats.1.total_output_bytes, "fn main() {}".len() as u64);
    }

    #[test]
    fn analyze_exec_breakdown() {
        let session = build_test_session(vec![
            make_assistant(vec![make_call("c1", "exec", r#"{"command":"git status"}"#)]),
            make_tool("c1", "ok"),
            make_assistant(vec![make_call("c2", "exec", r#"{"command":"git status"}"#)]),
            make_tool("c2", "clean"),
            make_assistant(vec![make_call("c3", "exec", r#"{"command":"cargo test"}"#)]),
            make_tool("c3", "test result: ok. 5 passed"),
        ]);

        let report = analyze(&[session]);

        let git_status = report
            .by_exec_cmd
            .iter()
            .find(|(n, _)| n == "git status")
            .unwrap();
        assert_eq!(git_status.1.calls, 2);
        assert_eq!(
            git_status.1.total_output_bytes,
            "ok".len() as u64 + "clean".len() as u64
        );

        let cargo_test = report
            .by_exec_cmd
            .iter()
            .find(|(n, _)| n == "cargo test")
            .unwrap();
        assert_eq!(cargo_test.1.calls, 1);
        assert_eq!(
            cargo_test.1.total_output_bytes,
            "test result: ok. 5 passed".len() as u64
        );
    }

    #[test]
    fn analyze_sorted_by_total_output_desc() {
        let session = build_test_session(vec![
            make_assistant(vec![make_call("c1", "exec", r#"{"command":"git diff"}"#)]),
            make_tool("c1", &"x".repeat(1000)),
            make_assistant(vec![make_call("c2", "file_read", r#"{"path":"f"}"#)]),
            make_tool("c2", &"y".repeat(500)),
            make_assistant(vec![make_call("c3", "exec", r#"{"command":"ls"}"#)]),
            make_tool("c3", &"z".repeat(2000)),
        ]);

        let report = analyze(&[session]);

        // exec total = 1000 + 2000 = 3000, file_read = 500
        assert_eq!(report.by_tool[0].0, "exec");
        assert_eq!(report.by_tool[1].0, "file_read");

        // exec breakdown: ls=2000, git diff=1000
        assert_eq!(report.by_exec_cmd[0].0, "ls");
        assert_eq!(report.by_exec_cmd[1].0, "git diff");
    }

    #[test]
    fn analyze_orphaned_tool_message_ignored() {
        let session = build_test_session(vec![make_tool(
            "orphan",
            "this has no matching assistant call",
        )]);

        let report = analyze(&[session]);
        assert!(report.by_tool.is_empty());
    }

    #[test]
    fn analyze_compacted_session() {
        // After compaction, session has a single System summary message.
        let session = build_test_session(vec![Message::System {
            content: "Summary of previous conversation.".to_string(),
        }]);

        let report = analyze(&[session]);
        assert!(report.by_tool.is_empty());
        assert!(report.by_exec_cmd.is_empty());
    }

    // ── format_report ─────────────────────────────────────────────

    #[test]
    fn format_report_empty() {
        let report = analyze(&[]);
        let out = format_report(&report);
        assert!(out.contains("Tool Usage (0 sessions)"));
        assert!(!out.contains("Exec Breakdown"));
    }

    #[test]
    fn format_report_includes_tool_and_exec_sections() {
        let session = build_test_session(vec![
            make_assistant(vec![make_call("c1", "exec", r#"{"command":"git status"}"#)]),
            make_tool("c1", &"x".repeat(2048)),
            make_assistant(vec![make_call("c2", "file_read", r#"{"path":"f"}"#)]),
            make_tool("c2", "short"),
        ]);
        let report = analyze(&[session]);
        let out = format_report(&report);

        assert!(out.contains("Tool Usage (1 session)"));
        assert!(out.contains("exec"));
        assert!(out.contains("file_read"));
        assert!(out.contains("2.0 KiB"));
        assert!(out.contains("Exec Breakdown"));
        assert!(out.contains("git status"));
    }

    #[test]
    fn format_report_no_exec_breakdown_without_exec_calls() {
        let session = build_test_session(vec![
            make_assistant(vec![make_call("c1", "file_read", r#"{"path":"f"}"#)]),
            make_tool("c1", "data"),
        ]);
        let report = analyze(&[session]);
        let out = format_report(&report);

        assert!(out.contains("file_read"));
        assert!(!out.contains("Exec Breakdown"));
    }

    #[test]
    fn analyze_multiple_sessions() {
        let s1 = build_test_session(vec![
            make_assistant(vec![make_call("c1", "exec", r#"{"command":"ls"}"#)]),
            make_tool("c1", "foo bar"),
        ]);
        let s2 = build_test_session(vec![
            make_assistant(vec![make_call("c1", "exec", r#"{"command":"ls"}"#)]),
            make_tool("c1", "baz"),
        ]);

        let report = analyze(&[s1, s2]);
        assert_eq!(report.session_count, 2);

        let exec = report.by_tool.iter().find(|(n, _)| n == "exec").unwrap();
        assert_eq!(exec.1.calls, 2);
        assert_eq!(
            exec.1.total_output_bytes,
            "foo bar".len() as u64 + "baz".len() as u64
        );
    }

    // ── classify_failure ─────────────────────────────────────────────

    #[test]
    fn classify_blocked() {
        assert_eq!(
            classify_failure("Error: Tool blocked: command blocked by policy"),
            Some(FailureKind::Blocked),
        );
    }

    #[test]
    fn classify_blocked_with_guidance() {
        assert_eq!(
            classify_failure("Error: Tool blocked: use the git_push tool"),
            Some(FailureKind::Blocked),
        );
    }

    #[test]
    fn classify_execution_failed() {
        assert_eq!(
            classify_failure("Error: Execution failed: No such file or directory"),
            Some(FailureKind::ExecutionFailed),
        );
    }

    #[test]
    fn classify_invalid_arguments() {
        assert_eq!(
            classify_failure("Error: Invalid arguments: missing field `command`"),
            Some(FailureKind::InvalidArguments),
        );
    }

    #[test]
    fn classify_not_found() {
        assert_eq!(
            classify_failure("Error: Tool not found: bogus_tool"),
            Some(FailureKind::NotFound),
        );
    }

    #[test]
    fn classify_timeout() {
        assert_eq!(
            classify_failure("Error: Tool execution timed out"),
            Some(FailureKind::Timeout),
        );
    }

    #[test]
    fn classify_safety_block() {
        assert_eq!(
            classify_failure(
                "Tool output blocked: Potential secret detected (pattern: OpenAI API key). Do not retry."
            ),
            Some(FailureKind::SafetyBlock),
        );
    }

    #[test]
    fn classify_repeat_block() {
        assert_eq!(
            classify_failure(
                "ERROR: You have called this tool with identical arguments multiple times"
            ),
            Some(FailureKind::RepeatBlock),
        );
    }

    #[test]
    fn classify_success_returns_none() {
        assert_eq!(
            classify_failure("<tool_output name=\"exec\">\nhello\n</tool_output>"),
            None,
        );
    }

    // ── truncate_str ─────────────────────────────────────────────────

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string() {
        let long = "a".repeat(70);
        let result = truncate_str(&long, 60);
        assert_eq!(result.len(), 60);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn truncate_exact_length() {
        let exact = "a".repeat(60);
        assert_eq!(truncate_str(&exact, 60), exact);
    }

    // ── analyze with failures ────────────────────────────────────────

    #[test]
    fn analyze_blocked_exec_command() {
        let session = build_test_session(vec![
            make_assistant(vec![make_call(
                "c1",
                "exec",
                r#"{"command":"git push origin main"}"#,
            )]),
            make_tool("c1", "Error: Tool blocked: use the git_push tool"),
            make_assistant(vec![make_call(
                "c2",
                "exec",
                r#"{"command":"git push origin dev"}"#,
            )]),
            make_tool("c2", "Error: Tool blocked: use the git_push tool"),
        ]);

        let report = analyze(&[session]);

        // Both have the same full command (different args but same start)
        // but extract_exec_full_command keeps the full string
        assert_eq!(report.blocked_cmds.len(), 2);
        let total: u64 = report.blocked_cmds.iter().map(|(_, c)| c).sum();
        assert_eq!(total, 2);

        // Still counted in by_tool
        let exec = report.by_tool.iter().find(|(n, _)| n == "exec").unwrap();
        assert_eq!(exec.1.calls, 2);
    }

    #[test]
    fn analyze_blocked_exec_same_command_aggregates() {
        let session = build_test_session(vec![
            make_assistant(vec![make_call(
                "c1",
                "exec",
                r#"{"command":"git push origin main"}"#,
            )]),
            make_tool("c1", "Error: Tool blocked: use the git_push tool"),
            make_assistant(vec![make_call(
                "c2",
                "exec",
                r#"{"command":"git push origin main"}"#,
            )]),
            make_tool("c2", "Error: Tool blocked: use the git_push tool"),
        ]);

        let report = analyze(&[session]);
        assert_eq!(report.blocked_cmds.len(), 1);
        assert_eq!(report.blocked_cmds[0].0, "git push origin main");
        assert_eq!(report.blocked_cmds[0].1, 2);
    }

    #[test]
    fn analyze_tool_error_timeout() {
        let session = build_test_session(vec![
            make_assistant(vec![make_call("c1", "exec", r#"{"command":"sleep 999"}"#)]),
            make_tool("c1", "Error: Tool execution timed out"),
        ]);

        let report = analyze(&[session]);
        assert_eq!(report.tool_errors.len(), 1);
        assert_eq!(report.tool_errors[0].0.tool, "exec");
        assert_eq!(report.tool_errors[0].0.kind, FailureKind::Timeout);
        assert_eq!(report.tool_errors[0].1, 1);
        // Not in blocked_cmds
        assert!(report.blocked_cmds.is_empty());
    }

    #[test]
    fn analyze_non_exec_blocked_goes_to_tool_errors() {
        let session = build_test_session(vec![
            make_assistant(vec![make_call(
                "c1",
                "file_read",
                r#"{"path":"../../etc/passwd"}"#,
            )]),
            make_tool("c1", "Error: Tool blocked: path traversal detected"),
        ]);

        let report = analyze(&[session]);
        assert!(report.blocked_cmds.is_empty());
        assert_eq!(report.tool_errors.len(), 1);
        assert_eq!(report.tool_errors[0].0.tool, "file_read");
        assert_eq!(report.tool_errors[0].0.kind, FailureKind::Blocked);
    }

    #[test]
    fn analyze_safety_block() {
        let session = build_test_session(vec![
            make_assistant(vec![make_call("c1", "exec", r#"{"command":"cat .env"}"#)]),
            make_tool(
                "c1",
                "Tool output blocked: Potential secret detected (pattern: OpenAI API key). Do not retry.",
            ),
        ]);

        let report = analyze(&[session]);
        assert!(report.blocked_cmds.is_empty());
        assert_eq!(report.tool_errors.len(), 1);
        assert_eq!(report.tool_errors[0].0.kind, FailureKind::SafetyBlock);
    }

    #[test]
    fn analyze_repeat_block() {
        let session = build_test_session(vec![
            make_assistant(vec![make_call("c1", "exec", r#"{"command":"git status"}"#)]),
            make_tool(
                "c1",
                "ERROR: You have called this tool with identical arguments multiple times and received the same result.",
            ),
        ]);

        let report = analyze(&[session]);
        assert_eq!(report.tool_errors.len(), 1);
        assert_eq!(report.tool_errors[0].0.kind, FailureKind::RepeatBlock);
    }

    #[test]
    fn analyze_mixed_success_and_failure() {
        let session = build_test_session(vec![
            make_assistant(vec![make_call("c1", "exec", r#"{"command":"ls"}"#)]),
            make_tool("c1", "<tool_output name=\"exec\">\nfiles\n</tool_output>"),
            make_assistant(vec![make_call(
                "c2",
                "exec",
                r#"{"command":"git push origin main"}"#,
            )]),
            make_tool("c2", "Error: Tool blocked: use the git_push tool"),
        ]);

        let report = analyze(&[session]);

        let exec = report.by_tool.iter().find(|(n, _)| n == "exec").unwrap();
        assert_eq!(exec.1.calls, 2);

        assert_eq!(report.blocked_cmds.len(), 1);
        assert!(report.tool_errors.is_empty());
    }

    // ── format_report with failures ──────────────────────────────────

    #[test]
    fn format_report_includes_blocked_section() {
        let session = build_test_session(vec![
            make_assistant(vec![make_call(
                "c1",
                "exec",
                r#"{"command":"git push origin main"}"#,
            )]),
            make_tool("c1", "Error: Tool blocked: use the git_push tool"),
        ]);
        let report = analyze(&[session]);
        let out = format_report(&report);

        assert!(out.contains("Blocked Commands"));
        assert!(out.contains("git push origin main"));
    }

    #[test]
    fn format_report_includes_tool_errors_section() {
        let session = build_test_session(vec![
            make_assistant(vec![make_call("c1", "exec", r#"{"command":"sleep 999"}"#)]),
            make_tool("c1", "Error: Tool execution timed out"),
        ]);
        let report = analyze(&[session]);
        let out = format_report(&report);

        assert!(out.contains("Tool Errors"));
        assert!(out.contains("exec"));
        assert!(out.contains("Timeout"));
    }

    #[test]
    fn format_report_no_failure_sections_when_all_succeed() {
        let session = build_test_session(vec![
            make_assistant(vec![make_call("c1", "exec", r#"{"command":"ls"}"#)]),
            make_tool("c1", "<tool_output name=\"exec\">\nok\n</tool_output>"),
        ]);
        let report = analyze(&[session]);
        let out = format_report(&report);

        assert!(!out.contains("Blocked Commands"));
        assert!(!out.contains("Tool Errors"));
    }
}
