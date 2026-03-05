//! Session statistics.
//!
//! Scans session files and reports which tools and exec commands consume
//! the most output bytes, guiding optimization work.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::path::Path;

use crate::session::Session;
use crate::types::Message;

// ── Domain types ────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct ToolStats {
    calls: u64,
    total_output_bytes: u64,
}

#[derive(Debug)]
struct Report {
    session_count: usize,
    by_tool: Vec<(String, ToolStats)>,
    by_exec_cmd: Vec<(String, ToolStats)>,
}

/// Extracted context from a tool call needed for attribution.
struct CallInfo {
    tool_name: String,
    exec_cmd: Option<String>,
}

// ── Public entry point ──────────────────────────────────────────────

pub fn run(workspace: &Path) {
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
    print!("{}", format_report(&report));
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
);

fn analyze(sessions: &[Session]) -> Report {
    let (_, by_tool, by_exec_cmd) = sessions.iter().flat_map(Session::messages).fold(
        <Acc>::default(),
        |(mut pending, mut by_tool, mut by_exec_cmd), msg| {
            match msg {
                Message::Assistant {
                    tool_calls: Some(calls),
                    ..
                } => {
                    let call_infos = calls.iter().map(|call| CallInfo {
                        tool_name: call.function.name.clone(),
                        exec_cmd: (call.function.name == "exec")
                            .then(|| extract_exec_command(&call.function.arguments)),
                    });
                    pending.extend(call_infos);
                }
                Message::Tool { content, .. } => {
                    if let Some(info) = pending.pop_front() {
                        let bytes = content.len() as u64;
                        accumulate(&mut by_tool, info.tool_name, bytes);
                        if let Some(cmd) = info.exec_cmd {
                            accumulate(&mut by_exec_cmd, cmd, bytes);
                        }
                    }
                }
                _ => {}
            }
            (pending, by_tool, by_exec_cmd)
        },
    );

    let sorted = |map: HashMap<String, ToolStats>| {
        let mut v: Vec<_> = map.into_iter().collect();
        v.sort_by(|a, b| b.1.total_output_bytes.cmp(&a.1.total_output_bytes));
        v
    };

    Report {
        session_count: sessions.len(),
        by_tool: sorted(by_tool),
        by_exec_cmd: sorted(by_exec_cmd),
    }
}

fn accumulate(map: &mut HashMap<String, ToolStats>, key: String, bytes: u64) {
    let entry = map.entry(key).or_default();
    entry.calls += 1;
    entry.total_output_bytes += bytes;
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

    out
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ToolCall, ToolFunction};

    fn make_assistant(calls: Vec<ToolCall>) -> Message {
        Message::Assistant {
            content: String::new(),
            tool_calls: Some(calls),
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
}
