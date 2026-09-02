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
    LazyLock::new(|| crate::text::static_regex(r"(?m)^=+ (.+?) =+$"));

static PYTEST_PAIR_RE: LazyLock<Regex> =
    LazyLock::new(|| crate::text::static_regex(r"(\d+) ([a-z]+)"));

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

/// Pytest short summary, summed across runs. Zero-count categories
/// are omitted from the line and their order varies, so every
/// `N category` pair on a summary bar is scanned: `passed` counts as
/// passed, `failed`/`error(s)` count as failures (a collection error
/// is a failure for evidence purposes), and everything else
/// (skipped, warnings, deselected) is ignored. Bars with no known
/// category, like the `=== FAILURES ===` section header, do not mark
/// the output as a pytest run.
fn pytest(output: &str) -> Option<Evidence> {
    let mut seen = false;
    let (mut passed, mut failed) = (0u64, 0u64);
    for line in PYTEST_RESULT_RE.captures_iter(output) {
        for pair in PYTEST_PAIR_RE.captures_iter(&line[1]) {
            let count = pair[1].parse::<u64>().unwrap_or(0);
            match &pair[2] {
                "passed" => {
                    seen = true;
                    passed += count;
                }
                "error" | "errors" | "failed" => {
                    seen = true;
                    failed += count;
                }
                _ => {}
            }
        }
    }
    seen.then(|| Evidence {
        passed,
        failed,
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

    /// Pytest omits zero-count categories: the all-fail summary has
    /// no passed count at all, and it is exactly the single-failing
    /// debugging run the trailer exists for.
    #[test]
    fn pytest_all_fail_summary_is_recognized() {
        let out = "FAILED tests/test_api.py::test_checkout - AssertionError\n\
                   =========== 3 failed in 0.12s ===========\n";
        let t = trailer(out).unwrap();
        assert!(t.contains("0 passed, 3 failed"), "{t}");
    }

    /// Error-only and mixed-category summaries: a collection error is
    /// a failure for evidence purposes, warnings and skips are not.
    #[test]
    fn pytest_error_categories_count_as_failures() {
        let t = trailer("=========== 1 error in 0.12s ===========\n").unwrap();
        assert!(t.contains("0 passed, 1 failed"), "{t}");

        let t = trailer("=========== 1 failed, 1 error, 2 skipped in 0.2s ===========\n").unwrap();
        assert!(t.contains("0 passed, 2 failed"), "{t}");
    }

    #[test]
    fn pytest_warnings_and_headers_are_not_failures() {
        assert_eq!(
            trailer("=========== 2 passed, 1 warning in 0.1s ===========\n"),
            None,
            "green run with warnings must get no trailer"
        );
        assert_eq!(trailer("=========== FAILURES ===========\n"), None);
    }

    #[test]
    fn pytest_counts_sum_across_runs() {
        let out = "=========== 1 failed, 2 passed in 0.10s ===========\n\
                   =========== 3 failed in 0.12s ===========\n";
        let t = trailer(out).unwrap();
        assert!(t.contains("2 passed, 4 failed"), "{t}");
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
