//! File editing tool.
//!
//! Find-and-replace editing with a four-rung match ladder: exact,
//! trailing-whitespace-insensitive, whitespace-flexible, and
//! Unicode-folded. Rungs are ordered least- to most-aggressive; the
//! first rung with at least one match decides the outcome.

use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;
use tracing::debug;

use super::path::PathGuard;
use super::{Tool, ToolCtx};
use crate::error::ToolError;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// File path relative to the workspace.
    path: String,
    /// The exact string to find. Must match exactly once.
    old_string: String,
    /// The replacement string. Empty string deletes the match.
    new_string: String,
}

/// Tool that performs find-and-replace edits on workspace files.
pub struct FileEdit {
    guard: PathGuard,
}

impl FileEdit {
    pub fn new(guard: PathGuard) -> Self {
        Self { guard }
    }
}

impl Tool for FileEdit {
    fn name(&self) -> &'static str {
        "file_edit"
    }

    fn description(&self) -> &'static str {
        "Find and replace a string in a file (must match exactly once)"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(Args)).expect("schema serialization failed")
    }

    fn execute(
        &self,
        args: serde_json::Value,
        _ctx: ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let args: Args = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

            if args.old_string.is_empty() {
                return Err(ToolError::InvalidArguments(
                    "old_string must be non-empty".into(),
                ));
            }

            let resolved = self.guard.resolve(&args.path)?;
            debug!(path = %args.path, "Editing file");
            let content = std::fs::read_to_string(&resolved)
                .map_err(|e| ToolError::ExecutionFailed(format!("{}: {e}", args.path)))?;

            let Some((rung, spans)) = find_matches(&content, &args.old_string) else {
                return Err(ToolError::ExecutionFailed(format!(
                    "no match found for old_string in {path}; the file may have \
                     changed since you read it. Current content:\n{content}",
                    path = args.path,
                )));
            };

            let (result, edited_at) = match spans.as_slice() {
                [only] => (
                    splice(&content, only.start, only.len, &args.new_string),
                    only.start,
                ),
                many => {
                    return Err(ToolError::ExecutionFailed(ambiguous_message(
                        rung, many, &content, &args.path,
                    )));
                }
            };

            std::fs::write(&resolved, &result)
                .map_err(|e| ToolError::ExecutionFailed(format!("{}: {e}", args.path)))?;

            let echo = echo_region(&result, edited_at, args.new_string.len());
            Ok(format!("Edited {}:\n{echo}", args.path))
        })
    }
}

/// A matched byte range in the original content.
struct Span {
    start: usize,
    len: usize,
}

type Normalizer = fn(&str) -> String;

/// Fuzzy rungs in escalation order, least- to most-aggressive.
const FUZZY_RUNGS: [(&str, Normalizer); 3] = [
    ("trailing-whitespace-insensitive", strip_trailing),
    ("whitespace-flexible", normalize_line),
    ("unicode-folded", fold_line),
];

/// Walk the match ladder. Returns the deciding rung's name and every
/// span it produced, or `None` when no rung matches.
fn find_matches(content: &str, old: &str) -> Option<(&'static str, Vec<Span>)> {
    let exact: Vec<Span> = content
        .match_indices(old)
        .map(|(start, m)| Span {
            start,
            len: m.len(),
        })
        .collect();
    if !exact.is_empty() {
        return Some(("exact", exact));
    }
    FUZZY_RUNGS.iter().find_map(|&(name, normalize)| {
        let spans = window_spans(content, old, normalize);
        (!spans.is_empty()).then_some((name, spans))
    })
}

/// Line-window matching under a normalizer. Comparison runs on
/// normalized views; the returned spans address the original bytes, so
/// the splice never rewrites content it wasn't asked to touch.
fn window_spans(content: &str, old: &str, normalize: Normalizer) -> Vec<Span> {
    let needle: Vec<String> = old.lines().map(normalize).collect();
    if needle.is_empty() {
        return Vec::new();
    }

    let lines: Vec<&str> = content.lines().collect();
    let window = needle.len();

    (0..lines.len().saturating_sub(window - 1))
        .filter(|&start| {
            lines[start..start + window]
                .iter()
                .zip(&needle)
                .all(|(c, n)| normalize(c) == *n)
        })
        .map(|start| {
            let start_byte = lines[..start].iter().map(|l| l.len() + 1).sum::<usize>();
            let len = lines[start..start + window]
                .iter()
                .enumerate()
                .map(|(i, l)| l.len() + usize::from(start + i + 1 < lines.len()))
                .sum();
            Span {
                start: start_byte,
                len,
            }
        })
        .collect()
}

/// Error text for a rung that matched more than once.
fn ambiguous_message(rung: &str, spans: &[Span], content: &str, path: &str) -> String {
    let lines: Vec<String> = spans
        .iter()
        .map(|s| line_of(content, s.start).to_string())
        .collect();
    format!(
        "{count} matches for old_string in {path} at lines {lines} ({rung} match); \
         add surrounding context to make it unique",
        count = spans.len(),
        lines = lines.join(", "),
    )
}

/// 1-based line number of a byte position.
fn line_of(content: &str, pos: usize) -> usize {
    content[..pos].bytes().filter(|&b| b == b'\n').count() + 1
}

/// Context lines on each side of an edited region in the success echo.
const CONTEXT_LINES: usize = 3;

/// Render the region at `[start, start + len)` with line numbers and
/// context, in `file_read`'s `line\tcontent` format.
fn echo_region(content: &str, start: usize, len: usize) -> String {
    use std::fmt::Write;

    let first = line_of(content, start);
    let last = if len == 0 {
        first
    } else {
        line_of(content, start + len - 1)
    };
    let from = first.saturating_sub(CONTEXT_LINES).max(1);
    let to = last + CONTEXT_LINES;

    content
        .lines()
        .enumerate()
        .skip(from - 1)
        .take(to - from + 1)
        .fold(String::new(), |mut acc, (i, line)| {
            let _ = writeln!(acc, "{}\t{line}", i + 1);
            acc
        })
}

/// Strip trailing whitespace only.
fn strip_trailing(s: &str) -> String {
    s.trim_end().to_string()
}

/// Collapse whitespace runs to single space, trim trailing whitespace.
fn normalize_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Fold typographic confusables to ASCII, then collapse whitespace.
fn fold_line(s: &str) -> String {
    normalize_line(&fold_unicode(s))
}

/// Map curly quotes, en/em dashes, and non-breaking spaces to ASCII.
fn fold_unicode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{201C}' | '\u{201D}' => '"',
            '\u{2013}' | '\u{2014}' => '-',
            '\u{00A0}' => ' ',
            other => other,
        })
        .collect()
}

/// Replace `len` bytes at `pos` with `replacement`.
fn splice(content: &str, pos: usize, len: usize, replacement: &str) -> String {
    let mut result = String::with_capacity(content.len() - len + replacement.len());
    result.push_str(&content[..pos]);
    result.push_str(replacement);
    result.push_str(&content[pos + len..]);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(content: &str) -> (tempfile::TempDir, FileEdit) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.txt"), content).unwrap();
        let guard = PathGuard::new(dir.path());
        (dir, FileEdit::new(guard))
    }

    fn read(dir: &tempfile::TempDir) -> String {
        std::fs::read_to_string(dir.path().join("test.txt")).unwrap()
    }

    async fn edit(
        tool: &FileEdit,
        old_string: &str,
        new_string: &str,
    ) -> Result<String, ToolError> {
        tool.execute(
            serde_json::json!({
                "path": "test.txt",
                "old_string": old_string,
                "new_string": new_string
            }),
            ToolCtx::default(),
        )
        .await
    }

    #[tokio::test]
    async fn single_replace() {
        let (dir, tool) = setup("hello world");
        let result = edit(&tool, "world", "rust").await.unwrap();
        assert!(result.contains("Edited"));
        assert_eq!(read(&dir), "hello rust");
    }

    #[tokio::test]
    async fn delete_via_empty_new_string() {
        let (dir, tool) = setup("hello cruel world");
        edit(&tool, "cruel ", "").await.unwrap();
        assert_eq!(read(&dir), "hello world");
    }

    #[tokio::test]
    async fn multiple_matches_error_lists_lines() {
        let (_dir, tool) = setup("a\nb\na\nb\na\n");
        let result = edit(&tool, "a", "x").await;
        match result {
            Err(ToolError::ExecutionFailed(msg)) => {
                assert!(msg.contains("3 matches"), "{msg}");
                assert!(msg.contains("lines 1, 3, 5"), "{msg}");
                assert!(msg.contains("exact match"), "{msg}");
            }
            other => panic!("expected ExecutionFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_match_error_carries_snapshot() {
        let (_dir, tool) = setup("hello world");
        let result = edit(&tool, "missing", "x").await;
        match result {
            Err(ToolError::ExecutionFailed(msg)) => {
                assert!(msg.contains("may have changed since you read it"), "{msg}");
                assert!(msg.contains("hello world"), "{msg}");
            }
            other => panic!("expected ExecutionFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn success_echoes_region_with_context() {
        let (_dir, tool) = setup("l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\n");
        let result = edit(&tool, "l5", "changed").await.unwrap();
        // Edited line plus three lines of context each side, numbered.
        assert!(result.contains("2\tl2"), "{result}");
        assert!(result.contains("5\tchanged"), "{result}");
        assert!(result.contains("8\tl8"), "{result}");
        assert!(!result.contains("1\tl1"), "{result}");
        assert!(!result.contains("9\tl9"), "{result}");
    }

    #[tokio::test]
    async fn echo_clamps_at_file_start() {
        let (_dir, tool) = setup("l1\nl2\n");
        let result = edit(&tool, "l1", "first").await.unwrap();
        assert!(result.contains("1\tfirst"), "{result}");
        assert!(result.contains("2\tl2"), "{result}");
    }

    #[tokio::test]
    async fn deletion_echoes_around_the_cut() {
        let (dir, tool) = setup("keep1\ngone\nkeep2\n");
        let result = edit(&tool, "gone\n", "").await.unwrap();
        assert_eq!(read(&dir), "keep1\nkeep2\n");
        assert!(result.contains("1\tkeep1"), "{result}");
        assert!(result.contains("2\tkeep2"), "{result}");
    }

    #[tokio::test]
    async fn trailing_whitespace_rung_matches() {
        let (dir, tool) = setup("foo();  \nbar();\n");
        edit(&tool, "foo();\nbar();", "baz();").await.unwrap();
        assert_eq!(read(&dir), "baz();\n");
    }

    #[tokio::test]
    async fn whitespace_flexible_match() {
        let (dir, tool) = setup("fn  main()  {\n    println!(\"hi\");\n}\n");
        edit(
            &tool,
            "fn main() {\n  println!(\"hi\");\n}",
            "fn main() {\n    println!(\"hello\");\n}",
        )
        .await
        .unwrap();
        let content = read(&dir);
        assert!(content.contains("hello"));
        assert!(!content.contains("hi"));
    }

    #[tokio::test]
    async fn unicode_fold_rung_matches() {
        let (dir, tool) = setup("let s = \u{201C}hi\u{201D}; // em\u{2014}dash\n");
        edit(&tool, "let s = \"hi\"; // em-dash", "let s = \"bye\";")
            .await
            .unwrap();
        assert_eq!(read(&dir), "let s = \"bye\";\n");
    }

    #[tokio::test]
    async fn exact_match_beats_folded_match() {
        let (dir, tool) = setup("let a = \u{201C}x\u{201D};\nlet a = \"x\";\n");
        edit(&tool, "let a = \"x\";", "let a = \"y\";")
            .await
            .unwrap();
        // The exact match on line 2 wins; the curly-quoted line 1 is untouched.
        assert_eq!(read(&dir), "let a = \u{201C}x\u{201D};\nlet a = \"y\";\n");
    }

    #[tokio::test]
    async fn ambiguous_fuzzy_match_errors_with_rung() {
        let (_dir, tool) = setup("foo( 1 );\nfoo(  1 );\n");
        let result = edit(&tool, "foo(   1 );", "bar( 1 );").await;
        match result {
            Err(ToolError::ExecutionFailed(msg)) => {
                assert!(msg.contains("2 matches"), "{msg}");
                assert!(msg.contains("whitespace-flexible match"), "{msg}");
                assert!(msg.contains("lines 1, 2"), "{msg}");
            }
            other => panic!("expected ExecutionFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_old_string_rejected() {
        let (_dir, tool) = setup("content");
        let result = edit(&tool, "", "x").await;
        assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
    }

    #[test]
    fn normalize_collapses_whitespace() {
        assert_eq!(normalize_line("  foo   bar  "), "foo bar");
        assert_eq!(normalize_line("\tbaz\t\tqux"), "baz qux");
        assert_eq!(normalize_line(""), "");
    }

    #[test]
    fn fold_maps_confusables() {
        assert_eq!(
            fold_unicode("\u{2018}a\u{2019} \u{201C}b\u{201D} \u{2013}\u{2014}\u{00A0}"),
            "'a' \"b\" -- "
        );
    }

    #[test]
    fn splice_replaces_correctly() {
        assert_eq!(splice("hello world", 6, 5, "rust"), "hello rust");
        assert_eq!(splice("abcdef", 2, 2, "XY"), "abXYef");
        assert_eq!(splice("abc", 1, 1, ""), "ac");
    }
}
