//! File editing tool.
//!
//! Find-and-replace editing with exact-once matching. Falls back to
//! whitespace-flexible matching when an exact match fails.

use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;
use tracing::debug;

use super::Tool;
use super::path::PathGuard;
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

            let result = match exact_replace(&content, &args.old_string, &args.new_string) {
                Ok(replaced) => replaced,
                Err(Some(msg)) => return Err(ToolError::ExecutionFailed(msg)),
                Err(None) => flexible_replace(&content, &args.old_string, &args.new_string)
                    .ok_or_else(|| {
                        ToolError::ExecutionFailed(format!(
                            "no match found for old_string in {}",
                            args.path
                        ))
                    })?,
            };

            std::fs::write(&resolved, &result)
                .map_err(|e| ToolError::ExecutionFailed(format!("{}: {e}", args.path)))?;

            Ok(format!("Edited {}", args.path))
        })
    }
}

/// Try exact `match_indices`. Returns `Ok(new_content)` if exactly one match,
/// `Err(None)` if zero matches (caller should try flexible), `Err(Some(msg))`
/// if multiple matches (caller should report to LLM).
fn exact_replace(content: &str, old: &str, new: &str) -> Result<String, Option<String>> {
    let mut iter = content.match_indices(old);
    let first = iter.next();
    match (first, iter.next()) {
        (None, _) => Err(None),
        (Some((pos, _)), None) => Ok(splice(content, pos, old.len(), new)),
        _ => {
            let count = 2 + iter.count();
            Err(Some(format!("{count} matches found, expected exactly 1")))
        }
    }
}

/// Whitespace-flexible fallback. Normalizes whitespace in both the content
/// and search string, then finds a unique match via sliding window over
/// the original content's lines.
fn flexible_replace(content: &str, old: &str, new: &str) -> Option<String> {
    let needle_lines: Vec<String> = old.lines().map(normalize_line).collect();
    if needle_lines.is_empty() {
        return None;
    }

    let content_lines: Vec<&str> = content.lines().collect();
    let window = needle_lines.len();

    let mut matches = (0..content_lines.len().saturating_sub(window - 1)).filter(|&start| {
        content_lines[start..start + window]
            .iter()
            .zip(&needle_lines)
            .all(|(c, n)| normalize_line(c) == *n)
    });

    let start_line = matches.next()?;

    // Must be exactly one match.
    if matches.next().is_some() {
        return None;
    }

    let end_line = start_line + window;

    let start_byte = content_lines[..start_line]
        .iter()
        .map(|l| l.len() + 1)
        .sum::<usize>();

    let match_byte_len: usize = content_lines[start_line..end_line]
        .iter()
        .enumerate()
        .map(|(i, l)| l.len() + usize::from(start_line + i + 1 < content_lines.len()))
        .sum();

    Some(splice(content, start_byte, match_byte_len, new))
}

/// Collapse whitespace runs to single space, trim trailing whitespace.
fn normalize_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
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

    #[tokio::test]
    async fn single_replace() {
        let (dir, tool) = setup("hello world");
        let result = tool
            .execute(serde_json::json!({
                "path": "test.txt",
                "old_string": "world",
                "new_string": "rust"
            }))
            .await
            .unwrap();
        assert!(result.contains("Edited"));
        assert_eq!(read(&dir), "hello rust");
    }

    #[tokio::test]
    async fn delete_via_empty_new_string() {
        let (dir, tool) = setup("hello cruel world");
        tool.execute(serde_json::json!({
            "path": "test.txt",
            "old_string": "cruel ",
            "new_string": ""
        }))
        .await
        .unwrap();
        assert_eq!(read(&dir), "hello world");
    }

    #[tokio::test]
    async fn multiple_matches_error() {
        let (_dir, tool) = setup("aaa");
        let result = tool
            .execute(serde_json::json!({
                "path": "test.txt",
                "old_string": "a",
                "new_string": "b"
            }))
            .await;
        match result {
            Err(ToolError::ExecutionFailed(msg)) => assert!(msg.contains("3 matches")),
            other => panic!("expected ExecutionFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_match_error() {
        let (_dir, tool) = setup("hello world");
        let result = tool
            .execute(serde_json::json!({
                "path": "test.txt",
                "old_string": "missing",
                "new_string": "x"
            }))
            .await;
        assert!(matches!(result, Err(ToolError::ExecutionFailed(_))));
    }

    #[tokio::test]
    async fn whitespace_flexible_match() {
        let (dir, tool) = setup("fn  main()  {\n    println!(\"hi\");\n}\n");
        tool.execute(serde_json::json!({
            "path": "test.txt",
            "old_string": "fn main() {\n  println!(\"hi\");\n}",
            "new_string": "fn main() {\n    println!(\"hello\");\n}"
        }))
        .await
        .unwrap();
        let content = read(&dir);
        assert!(content.contains("hello"));
        assert!(!content.contains("hi"));
    }

    #[tokio::test]
    async fn empty_old_string_rejected() {
        let (_dir, tool) = setup("content");
        let result = tool
            .execute(serde_json::json!({
                "path": "test.txt",
                "old_string": "",
                "new_string": "x"
            }))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
    }

    #[test]
    fn normalize_collapses_whitespace() {
        assert_eq!(normalize_line("  foo   bar  "), "foo bar");
        assert_eq!(normalize_line("\tbaz\t\tqux"), "baz qux");
        assert_eq!(normalize_line(""), "");
    }

    #[test]
    fn splice_replaces_correctly() {
        assert_eq!(splice("hello world", 6, 5, "rust"), "hello rust");
        assert_eq!(splice("abcdef", 2, 2, "XY"), "abXYef");
        assert_eq!(splice("abc", 1, 1, ""), "ac");
    }
}
