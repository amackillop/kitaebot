//! Event-triggered corrective hints (spec 01).
//!
//! Standing prompt guidance does not reach a model mid-habit; the
//! instruction has to arrive when its trigger fires. Each recognized
//! failure shape yields one system-notice line, at most once per turn.

use std::collections::{BTreeMap, BTreeSet};

/// A failed assertion in tool output.
pub const ASSERT_HINT: &str = "Hint: a test assertion failed. Derive the \
    expected value by hand from the inputs before changing code: the \
    test's own expectation may be the bug.";

/// The same test command re-run with no file edit in between.
pub const RERUN_HINT: &str = "Hint: that test command already ran and no \
    file_edit or file_write has happened since; the result will not \
    change. Edit something first or ask a different question.";

/// Repeated `file_edit` no-match on one file.
pub const STALE_EDIT_HINT: &str = "Hint: file_edit failed to match twice \
    on that file; your view of it is stale. Re-read the region before \
    retrying.";

/// Substrings marking a command as a test run. Multi-language on
/// purpose: the bot works non-Rust repos.
const TEST_COMMANDS: &[&str] = &[
    "cargo test",
    "go test",
    "just check",
    "just rust-check",
    "just test",
    "npm test",
    "pnpm test",
    "pytest",
    "vitest",
];

/// The one-shot hint kinds; each fires at most once per turn.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Assert,
    Rerun,
    StaleEdit,
}

/// Per-turn state for the corrective hints. Purely observational:
/// callers decide where the returned lines go.
pub struct HintTracker {
    fired: BTreeSet<Kind>,
    /// Last test command seen, and whether a file mutation happened
    /// after it.
    last_test: Option<String>,
    edited_since_test: bool,
    /// `file_edit` no-match counts per path.
    no_match: BTreeMap<String, usize>,
}

impl HintTracker {
    pub fn new() -> Self {
        Self {
            fired: BTreeSet::new(),
            last_test: None,
            edited_since_test: true,
            no_match: BTreeMap::new(),
        }
    }

    /// Latch `kind`: the text the first time, `None` after.
    fn fire(&mut self, kind: Kind, text: &'static str) -> Option<&'static str> {
        self.fired.insert(kind).then_some(text)
    }

    /// Observe one completed tool call. Returns a hint the first time
    /// each failure shape fires in the turn.
    pub fn observe(
        &mut self,
        tool: &str,
        args: &str,
        result: Result<&str, &str>,
    ) -> Option<&'static str> {
        match tool {
            "file_edit" | "file_write" => {
                if result.is_ok() {
                    self.edited_since_test = true;
                } else if tool == "file_edit"
                    && let Err(e) = result
                    && e.contains("no match found for old_string")
                {
                    let path = arg_str(args, "path").unwrap_or_default();
                    let count = self.no_match.entry(path).or_insert(0);
                    *count += 1;
                    if *count >= 2 {
                        return self.fire(Kind::StaleEdit, STALE_EDIT_HINT);
                    }
                }
            }
            "exec" => {
                if let Some(command) = arg_str(args, "command")
                    && TEST_COMMANDS.iter().any(|t| command.contains(t))
                {
                    let rerun = self.last_test.as_deref() == Some(command.as_str())
                        && !self.edited_since_test;
                    self.last_test = Some(command);
                    self.edited_since_test = false;
                    if rerun {
                        return self.fire(Kind::Rerun, RERUN_HINT);
                    }
                }
            }
            _ => {}
        }

        let text = result.unwrap_or_else(|e| e);
        if assertion_failed(text) {
            return self.fire(Kind::Assert, ASSERT_HINT);
        }
        None
    }
}

/// Failed-assertion shape across libtest vintages and languages.
fn assertion_failed(text: &str) -> bool {
    (text.contains("assertion") && text.contains("failed")) || text.contains("panicked at")
}

/// Extract a string field from a tool-call arguments JSON blob.
fn arg_str(args: &str, key: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(args).ok()?;
    value.get(key)?.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EDIT_OK: Result<&str, &str> = Ok("edited");
    const NO_MATCH: Result<&str, &str> = Err(
        "precondition failed: no match found for old_string in src/a.rs; the file may have changed",
    );

    fn exec_args(cmd: &str) -> String {
        serde_json::json!({ "command": cmd }).to_string()
    }

    #[test]
    fn assertion_failure_hints_once() {
        let mut h = HintTracker::new();
        let out = "thread 'x' panicked at src/a.rs:1:\nassertion `left == right` failed";
        assert_eq!(
            h.observe("exec", &exec_args("ls"), Ok(out)),
            Some(ASSERT_HINT)
        );
        assert_eq!(h.observe("exec", &exec_args("ls"), Ok(out)), None);
    }

    #[test]
    fn rerun_without_edit_hints_once() {
        let mut h = HintTracker::new();
        let args = exec_args("just test-one tools::exec");
        assert_eq!(h.observe("exec", &args, Ok("test result: ok")), None);
        assert_eq!(
            h.observe("exec", &args, Ok("test result: ok")),
            Some(RERUN_HINT)
        );
        assert_eq!(h.observe("exec", &args, Ok("test result: ok")), None);
    }

    #[test]
    fn edit_between_runs_suppresses_rerun_hint() {
        let mut h = HintTracker::new();
        let args = exec_args("cargo test foo");
        assert_eq!(h.observe("exec", &args, Ok("ok")), None);
        assert_eq!(h.observe("file_edit", "{\"path\":\"a.rs\"}", EDIT_OK), None);
        assert_eq!(h.observe("exec", &args, Ok("ok")), None);
    }

    #[test]
    fn different_test_command_is_not_a_rerun() {
        let mut h = HintTracker::new();
        assert_eq!(h.observe("exec", &exec_args("pytest a"), Ok("ok")), None);
        assert_eq!(h.observe("exec", &exec_args("pytest b"), Ok("ok")), None);
    }

    #[test]
    fn second_no_match_on_same_file_hints_once() {
        let mut h = HintTracker::new();
        let args = "{\"path\":\"src/a.rs\",\"old_string\":\"x\",\"new_string\":\"y\"}";
        assert_eq!(h.observe("file_edit", args, NO_MATCH), None);
        assert_eq!(
            h.observe("file_edit", args, NO_MATCH),
            Some(STALE_EDIT_HINT)
        );
        assert_eq!(h.observe("file_edit", args, NO_MATCH), None);
    }

    #[test]
    fn non_test_exec_and_clean_output_hint_nothing() {
        let mut h = HintTracker::new();
        assert_eq!(
            h.observe("exec", &exec_args("git status"), Ok("clean")),
            None
        );
        assert_eq!(h.observe("grep", "{}", Ok("no assertion here")), None);
    }
}
