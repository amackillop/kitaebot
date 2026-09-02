//! Mechanical test-evidence trailer for exec output (issue #145).
//!
//! The model reads test results through self-authored filters and
//! discards the decisive lines; a recognized test run gets a parsed
//! trailer appended so the verdict survives whatever the pipeline
//! dropped. Recognition is a per-format registry that degrades to no
//! trailer on unknown output, never to an error.

use std::fmt::Write as _;
use std::sync::LazyLock;

use regex::Regex;

/// Failed test names listed before the trailer truncates.
const MAX_FAILED_NAMES: usize = 10;

/// Lines of the first panic block carried into the trailer.
const PANIC_BLOCK_LINES: usize = 6;

static LIBTEST_RESULT_RE: LazyLock<Regex> = LazyLock::new(|| {
    crate::text::static_regex(r"(?m)^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed;")
});

static LIBTEST_FAILED_RE: LazyLock<Regex> =
    LazyLock::new(|| crate::text::static_regex(r"(?m)^test (\S+) \.\.\. FAILED$"));

static PYTEST_RESULT_RE: LazyLock<Regex> =
    LazyLock::new(|| crate::text::static_regex(r"(?m)^=+ .*?(\d+) failed, (\d+) passed.*? =+$"));

static PYTEST_FAILED_RE: LazyLock<Regex> =
    LazyLock::new(|| crate::text::static_regex(r"(?m)^FAILED (\S+)"));

/// Parsed totals and failures from one recognized format.
struct Evidence {
    passed: u64,
    failed: u64,
    failed_names: Vec<String>,
}

/// Append-ready trailer for `output`, when it is a recognized test
/// run with failures. Passing runs and unrecognized formats yield
/// `None`: the trailer exists to preserve failure evidence, not to
/// restate a green summary line.
pub fn trailer(output: &str) -> Option<String> {
    let evidence = libtest(output).or_else(|| pytest(output))?;
    if evidence.failed == 0 {
        return None;
    }
    let mut t = format!(
        "--- parsed test evidence ---\n{} passed, {} failed",
        evidence.passed, evidence.failed
    );
    if !evidence.failed_names.is_empty() {
        let shown = evidence.failed_names.len().min(MAX_FAILED_NAMES);
        let _ = write!(t, "\nfailed: {}", evidence.failed_names[..shown].join(", "));
        if evidence.failed_names.len() > shown {
            let _ = write!(t, " (+{} more)", evidence.failed_names.len() - shown);
        }
    }
    if let Some(panic) = first_panic(output) {
        let _ = write!(t, "\nfirst failure:\n{panic}");
    }
    Some(t)
}

/// Sum libtest `test result:` lines across suites.
fn libtest(output: &str) -> Option<Evidence> {
    let mut seen = false;
    let (mut passed, mut failed) = (0u64, 0u64);
    for cap in LIBTEST_RESULT_RE.captures_iter(output) {
        seen = true;
        passed += cap[2].parse::<u64>().unwrap_or(0);
        failed += cap[3].parse::<u64>().unwrap_or(0);
    }
    seen.then(|| Evidence {
        passed,
        failed,
        failed_names: LIBTEST_FAILED_RE
            .captures_iter(output)
            .map(|c| c[1].to_string())
            .collect(),
    })
}

/// Pytest short summary: `=== 2 failed, 5 passed in 0.31s ===`.
fn pytest(output: &str) -> Option<Evidence> {
    let cap = PYTEST_RESULT_RE.captures(output)?;
    Some(Evidence {
        passed: cap[2].parse().unwrap_or(0),
        failed: cap[1].parse().unwrap_or(0),
        failed_names: PYTEST_FAILED_RE
            .captures_iter(output)
            .map(|c| c[1].to_string())
            .collect(),
    })
}

/// First panic block: the `panicked at` line and the assertion lines
/// after it, where the left/right values live.
fn first_panic(output: &str) -> Option<String> {
    let start = output.find("panicked at")?;
    let block: Vec<&str> = output
        .get(start..)?
        .lines()
        .take(PANIC_BLOCK_LINES)
        .take_while(|l| !l.starts_with("note:"))
        .collect();
    Some(block.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIBTEST_FAIL: &str = "\
running 2 tests
test tools::exec::tests::a ... ok
test tools::exec::tests::b ... FAILED

failures:

---- tools::exec::tests::b stdout ----

thread 'tools::exec::tests::b' panicked at src/tools/exec.rs:1869:9:
assertion `left == right` failed
  left: Some(\"guidance\")
 right: None
note: run with `RUST_BACKTRACE=1` for a backtrace

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
";

    #[test]
    fn libtest_failure_yields_counts_names_and_values() {
        let t = trailer(LIBTEST_FAIL).unwrap();
        assert!(t.contains("1 passed, 1 failed"), "{t}");
        assert!(t.contains("failed: tools::exec::tests::b"), "{t}");
        assert!(t.contains("left: Some(\"guidance\")"), "{t}");
        assert!(t.contains("right: None"), "{t}");
        assert!(!t.contains("note: run with"), "{t}");
    }

    #[test]
    fn passing_run_gets_no_trailer() {
        let out = "test a ... ok\n\ntest result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n";
        assert_eq!(trailer(out), None);
    }

    #[test]
    fn multi_suite_counts_sum() {
        let out = "test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n\
                   test result: FAILED. 2 passed; 4 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n";
        let t = trailer(out).unwrap();
        assert!(t.contains("5 passed, 4 failed"), "{t}");
    }

    #[test]
    fn pytest_summary_is_recognized() {
        let out = "FAILED tests/test_api.py::test_checkout - AssertionError\n\
                   =========== 1 failed, 12 passed in 0.31s ===========\n";
        let t = trailer(out).unwrap();
        assert!(t.contains("12 passed, 1 failed"), "{t}");
        assert!(t.contains("tests/test_api.py::test_checkout"), "{t}");
    }

    #[test]
    fn unrecognized_output_yields_nothing() {
        assert_eq!(
            trailer("Compiling kitaebot v0.0.1\nerror[E0308]: mismatched types"),
            None
        );
        assert_eq!(trailer(""), None);
    }

    #[test]
    fn failed_name_list_truncates() {
        let names = (0..15).fold(String::new(), |mut s, i| {
            let _ = writeln!(s, "test m::t{i} ... FAILED");
            s
        });
        let out = format!(
            "{names}\ntest result: FAILED. 0 passed; 15 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n"
        );
        let t = trailer(&out).unwrap();
        assert!(t.contains("(+5 more)"), "{t}");
    }
}
