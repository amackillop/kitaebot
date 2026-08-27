//! File editing tool.
//!
//! Find-and-replace editing with a four-rung match ladder: exact,
//! trailing-whitespace-insensitive, whitespace-flexible, and
//! Unicode-folded. Rungs are ordered least- to most-aggressive; the
//! first rung with at least one match decides the outcome.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::pin::Pin;
use std::sync::Mutex;

use schemars::JsonSchema;
use serde::Deserialize;
use tracing::debug;

use super::path::PathGuard;
use super::{Tool, ToolCtx, truncate_output};
use crate::error::{EditFutility, ToolError};

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// File path relative to the workspace.
    path: String,
    /// The exact string to find. Must match exactly once, unless
    /// `replace_all` is set.
    old_string: String,
    /// The replacement string. Empty string deletes the match. Must
    /// differ from `old_string`.
    new_string: String,
    /// Replace every exact occurrence of `old_string`. Only applies to
    /// exact matches; fuzzy matches always require a unique target.
    #[serde(default)]
    replace_all: bool,
}

/// Tool that performs find-and-replace edits on workspace files.
pub struct FileEdit {
    guard: PathGuard,
    history: Mutex<EditHistory>,
}

impl FileEdit {
    pub fn new(guard: PathGuard) -> Self {
        Self {
            guard,
            history: Mutex::new(EditHistory::default()),
        }
    }

    /// Record a futile attempt; the third identical payload in the
    /// path's recent window becomes a hard stop.
    fn record_futile(
        &self,
        path: &str,
        fingerprint: u64,
        outcome: EditFutility,
    ) -> Result<(), ToolError> {
        let attempts = self
            .history
            .lock()
            .expect("edit history mutex poisoned")
            .record_futile(path, fingerprint);
        if attempts >= FUTILE_LIMIT {
            return Err(ToolError::EditLoop {
                path: path.to_string(),
                attempts,
                outcome,
            });
        }
        Ok(())
    }
}

impl Tool for FileEdit {
    fn name(&self) -> &'static str {
        "file_edit"
    }

    fn description(&self) -> &'static str {
        "Find and replace a string in a file. old_string must match exactly \
         once; set replace_all to replace every exact occurrence instead. \
         Whitespace and typographic-punctuation differences are tolerated \
         when the match is unique."
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
            if args.old_string == args.new_string {
                return Err(ToolError::InvalidArguments(
                    "old_string and new_string are identical; the edit would be a no-op".into(),
                ));
            }

            let resolved = self.guard.resolve_writable(&args.path)?;
            debug!(path = %args.path, "Editing file");
            let content = std::fs::read_to_string(&resolved).map_err(|e| ToolError::Io {
                operation: "read",
                path: (&args.path).into(),
                source: e,
            })?;

            let fingerprint = fingerprint(&args);
            let Some((rung, spans)) = find_matches(&content, &args.old_string) else {
                self.record_futile(&args.path, fingerprint, EditFutility::FailedIdentically)?;
                return Err(ToolError::Precondition(stale_read_message(
                    &content,
                    &args.old_string,
                    &args.path,
                )));
            };

            let (result, edited_at, replaced) = match spans.as_slice() {
                [only] => (
                    splice(&content, only.start, only.len, &args.new_string),
                    only.start,
                    1,
                ),
                many if rung == Rung::Exact && args.replace_all => (
                    splice_all(&content, many, &args.new_string),
                    many[0].start,
                    many.len(),
                ),
                many => {
                    self.record_futile(&args.path, fingerprint, EditFutility::FailedIdentically)?;
                    return Err(ToolError::Precondition(ambiguous_message(
                        rung, many, &content, &args.path,
                    )));
                }
            };

            if result == content {
                self.record_futile(&args.path, fingerprint, EditFutility::NoChange)?;
                let echo = echo_region(&result, edited_at, args.new_string.len());
                return Ok(format!("Edited {} (no change):\n{echo}", args.path));
            }

            self.history
                .lock()
                .expect("edit history mutex poisoned")
                .clear(&args.path);
            std::fs::write(&resolved, &result).map_err(|e| ToolError::Io {
                operation: "write",
                path: (&args.path).into(),
                source: e,
            })?;

            let echo = echo_region(&result, edited_at, args.new_string.len());
            if replaced > 1 {
                Ok(format!(
                    "Edited {} ({replaced} replacements):\n{echo}",
                    args.path
                ))
            } else {
                Ok(format!("Edited {}:\n{echo}", args.path))
            }
        })
    }
}

/// Attempts of one identical futile payload that trip the guard.
const FUTILE_LIMIT: usize = 3;

/// Futile payloads remembered per path.
const RECENT_WINDOW: usize = 8;

/// Paths tracked before the history resets. A heuristic guard, not a
/// ledger: losing counts on overflow only delays a hard stop.
const MAX_PATHS: usize = 32;

/// Recent futile edit payloads per path — attempts that failed to
/// match or matched without changing the file. A ring rather than a
/// consecutive counter, so interleaving two failing payloads still
/// trips the guard for each.
#[derive(Default)]
struct EditHistory(HashMap<String, VecDeque<u64>>);

impl EditHistory {
    /// Record a futile attempt; returns how many times this
    /// fingerprint appears in the path's recent window, this one
    /// included.
    fn record_futile(&mut self, path: &str, fingerprint: u64) -> usize {
        if self.0.len() >= MAX_PATHS && !self.0.contains_key(path) {
            self.0.clear();
        }
        let ring = self.0.entry(path.to_string()).or_default();
        if ring.len() >= RECENT_WINDOW {
            ring.pop_front();
        }
        ring.push_back(fingerprint);
        ring.iter().filter(|&&f| f == fingerprint).count()
    }

    /// A content-changing edit landed; prior futility is stale.
    fn clear(&mut self, path: &str) {
        self.0.remove(path);
    }
}

/// Collision-tolerant payload identity: a false positive fires the
/// guard early with a re-read instruction, which is harmless.
fn fingerprint(args: &Args) -> u64 {
    let mut hasher = DefaultHasher::new();
    (&args.old_string, &args.new_string, args.replace_all).hash(&mut hasher);
    hasher.finish()
}

/// A matched byte range in the original content.
struct Span {
    start: usize,
    len: usize,
}

/// The ladder rung that decided a match.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Rung {
    Exact,
    Fuzzy(&'static str),
}

impl Rung {
    fn name(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Fuzzy(name) => name,
        }
    }
}

type Normalizer = fn(&str) -> String;

/// Fuzzy rungs in escalation order, least- to most-aggressive.
const FUZZY_RUNGS: [(&str, Normalizer); 3] = [
    ("trailing-whitespace-insensitive", strip_trailing),
    ("whitespace-flexible", normalize_line),
    ("unicode-folded", fold_line),
];

/// Walk the match ladder. Returns the deciding rung and every span it
/// produced, or `None` when no rung matches.
fn find_matches(content: &str, old: &str) -> Option<(Rung, Vec<Span>)> {
    let exact: Vec<Span> = content
        .match_indices(old)
        .map(|(start, m)| Span {
            start,
            len: m.len(),
        })
        .collect();
    if !exact.is_empty() {
        return Some((Rung::Exact, exact));
    }
    FUZZY_RUNGS.iter().find_map(|&(name, normalize)| {
        let spans = window_spans(content, old, normalize);
        (!spans.is_empty()).then_some((Rung::Fuzzy(name), spans))
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
fn ambiguous_message(rung: Rung, spans: &[Span], content: &str, path: &str) -> String {
    let lines: Vec<String> = spans
        .iter()
        .map(|s| line_of(content, s.start).to_string())
        .collect();
    let suggestion = match rung {
        Rung::Exact => "add surrounding context to make it unique, or set replace_all",
        Rung::Fuzzy(_) => "add surrounding context to make it unique",
    };
    format!(
        "{count} matches for old_string in {path} at lines {lines} ({rung} match); \
         {suggestion}",
        count = spans.len(),
        lines = lines.join(", "),
        rung = rung.name(),
    )
}

/// Byte cap on the stale-read snapshot. The model needs enough to
/// re-synchronize the edit, not the whole file; files that large are
/// stubbed out of context by the engine anyway (spec 14).
const SNAPSHOT_MAX_BYTES: usize = 2048;

/// Error text for a no-match on every rung. Carries the stale-read
/// hint plus a bounded excerpt around the closest candidate line, not
/// the whole file: the whole-file embed drowned the error tee on large
/// files (spec 24, entry size is correctness), and an excerpt around
/// the nearest match beats a full dump for re-synchronization anyway.
fn stale_read_message(content: &str, old: &str, path: &str) -> String {
    let excerpt = match nearest_line(content, old) {
        Some(line) => excerpt_around(content, line),
        None => format!("({} bytes, no lines)", content.len()),
    };
    format!(
        "no match found for old_string in {path}; the file may have \
         changed since you read it. Nearest candidate:\n{}",
        truncate_output(&excerpt, SNAPSHOT_MAX_BYTES),
    )
}

/// 1-based line number of the line most similar to `old`, by
/// normalized token overlap. Ties go to the earliest line; zero
/// overlap anywhere anchors at the top. `None` only when the file
/// has no lines at all.
fn nearest_line(content: &str, old: &str) -> Option<usize> {
    let normalized_old = normalize_line(old);
    let needle: HashSet<&str> = normalized_old
        .split(' ')
        .filter(|t| !t.is_empty())
        .collect();
    content
        .lines()
        .map(|line| {
            let normalized = normalize_line(line);
            let tokens: HashSet<&str> = normalized.split(' ').collect();
            needle.intersection(&tokens).count()
        })
        .enumerate()
        .max_by_key(|&(i, overlap)| (overlap, Reverse(i)))
        .map(|(i, overlap)| if overlap > 0 { i + 1 } else { 1 })
}

/// Numbered lines around `center` (inclusive), in `file_read`'s
/// `line\tcontent` format. A header line names the window so the
/// model can tell a partial excerpt from the whole file; it leads the
/// excerpt so the byte cap cannot cut it off.
fn excerpt_around(content: &str, center: usize) -> String {
    use std::fmt::Write;

    let total = content.lines().count();
    let from = center.saturating_sub(CONTEXT_LINES).max(1);
    let to = (center + CONTEXT_LINES).min(total);

    let mut out = String::new();
    if from > 1 || to < total {
        let _ = writeln!(out, "[showing lines {from}-{to} of {total}]");
    }
    let _ = write!(out, "{}", numbered_lines(content, from, to));
    out
}

/// 1-based line number of a byte position.
fn line_of(content: &str, pos: usize) -> usize {
    content[..pos].bytes().filter(|&b| b == b'\n').count() + 1
}

/// Context lines on each side of an edited region in the success echo.
const CONTEXT_LINES: usize = 3;

/// Numbered lines `from..=to` (1-based, inclusive), in `file_read`'s
/// `line\tcontent` format. `to` clamps to the last line.
fn numbered_lines(content: &str, from: usize, to: usize) -> String {
    use std::fmt::Write;

    let to = to.min(content.lines().count());
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

/// Render the region at `[start, start + len)` with line numbers and
/// context, in `file_read`'s `line\tcontent` format.
fn echo_region(content: &str, start: usize, len: usize) -> String {
    let first = line_of(content, start);
    let last = if len == 0 {
        first
    } else {
        line_of(content, start + len - 1)
    };
    numbered_lines(
        content,
        first.saturating_sub(CONTEXT_LINES).max(1),
        last + CONTEXT_LINES,
    )
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

/// Replace every span with `replacement`. Spans arrive ascending and
/// non-overlapping from `match_indices`; splicing back-to-front keeps
/// earlier offsets valid.
fn splice_all(content: &str, spans: &[Span], replacement: &str) -> String {
    let mut result = content.to_string();
    for span in spans.iter().rev() {
        result.replace_range(span.start..span.start + span.len, replacement);
    }
    result
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
            Err(ToolError::Precondition(msg)) => {
                assert!(msg.contains("3 matches"), "{msg}");
                assert!(msg.contains("lines 1, 3, 5"), "{msg}");
                assert!(msg.contains("exact match"), "{msg}");
            }
            other => panic!("expected Precondition, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_match_error_carries_snapshot() {
        let (_dir, tool) = setup("hello world");
        let result = edit(&tool, "missing", "x").await;
        match result {
            Err(ToolError::Precondition(msg)) => {
                assert!(msg.contains("may have changed since you read it"), "{msg}");
                assert!(msg.contains("hello world"), "{msg}");
            }
            other => panic!("expected Precondition, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_match_excerpt_anchors_on_nearest_candidate() {
        // Line 5 shares "fn" and "{" with the needle; every other line
        // shares nothing. The excerpt must center on line 5, not dump
        // the file.
        let content = "mod a;\nmod b;\nmod c;\nmod d;\nfn stale_read_message() {\n}\n\
                       mod e;\nmod f;\nmod g;\n";
        let (_dir, tool) = setup(content);
        let result = edit(&tool, "fn stale_read_message(&self) {", "x").await;
        match result {
            Err(ToolError::Precondition(msg)) => {
                assert!(msg.contains("[showing lines 2-8 of 9]"), "{msg}");
                assert!(msg.contains("5\tfn stale_read_message() {"), "{msg}");
                assert!(!msg.contains("1\tmod a;"), "{msg}");
                assert!(!msg.contains("9\tmod g;"), "{msg}");
            }
            other => panic!("expected Precondition, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_match_excerpt_is_bounded_on_large_files() {
        // No token overlap anywhere: the excerpt anchors at the top and
        // the long lines push it past the byte cap.
        let content = format!("{}\n", "x".repeat(600)).repeat(100);
        let (_dir, tool) = setup(&content);
        let result = edit(&tool, "fn stale_read_message(&self) {", "x").await;
        match result {
            Err(ToolError::Precondition(msg)) => {
                assert!(
                    msg.len() <= SNAPSHOT_MAX_BYTES + 256,
                    "msg is {} bytes",
                    msg.len()
                );
                assert!(msg.contains("[showing lines 1-4 of 100]"), "{msg}");
                assert!(msg.contains("[truncated"), "{msg}");
            }
            other => panic!("expected Precondition, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_match_with_zero_overlap_anchors_at_top() {
        let (_dir, tool) = setup("alpha\nbeta\n");
        let result = edit(&tool, "zzz qqq", "x").await;
        match result {
            Err(ToolError::Precondition(msg)) => {
                assert!(msg.contains("1\talpha"), "{msg}");
                assert!(msg.contains("2\tbeta"), "{msg}");
                assert!(!msg.contains('['), "{msg}");
            }
            other => panic!("expected Precondition, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_match_on_empty_file_names_byte_count() {
        let (_dir, tool) = setup("");
        let result = edit(&tool, "anything", "x").await;
        match result {
            Err(ToolError::Precondition(msg)) => {
                assert!(msg.contains("(0 bytes, no lines)"), "{msg}");
            }
            other => panic!("expected Precondition, got {other:?}"),
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
            Err(ToolError::Precondition(msg)) => {
                assert!(msg.contains("2 matches"), "{msg}");
                assert!(msg.contains("whitespace-flexible match"), "{msg}");
                assert!(msg.contains("lines 1, 2"), "{msg}");
            }
            other => panic!("expected Precondition, got {other:?}"),
        }
    }

    async fn edit_all(
        tool: &FileEdit,
        old_string: &str,
        new_string: &str,
    ) -> Result<String, ToolError> {
        tool.execute(
            serde_json::json!({
                "path": "test.txt",
                "old_string": old_string,
                "new_string": new_string,
                "replace_all": true
            }),
            ToolCtx::default(),
        )
        .await
    }

    #[tokio::test]
    async fn replace_all_replaces_every_exact_match() {
        let (dir, tool) = setup("a b\na c\na d\n");
        let result = edit_all(&tool, "a ", "x ").await.unwrap();
        assert_eq!(read(&dir), "x b\nx c\nx d\n");
        assert!(result.contains("3 replacements"), "{result}");
    }

    #[tokio::test]
    async fn replace_all_with_single_match_behaves_normally() {
        let (dir, tool) = setup("hello world");
        let result = edit_all(&tool, "world", "rust").await.unwrap();
        assert_eq!(read(&dir), "hello rust");
        assert!(!result.contains("replacements"), "{result}");
    }

    #[tokio::test]
    async fn replace_all_never_applies_to_fuzzy_matches() {
        let (_dir, tool) = setup("foo( 1 );\nfoo(  1 );\n");
        let result = edit_all(&tool, "foo(   1 );", "bar( 1 );").await;
        match result {
            Err(ToolError::Precondition(msg)) => {
                assert!(msg.contains("2 matches"), "{msg}");
                assert!(!msg.contains("or set replace_all"), "{msg}");
            }
            other => panic!("expected Precondition, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ambiguous_exact_match_suggests_replace_all() {
        let (_dir, tool) = setup("a\nb\na\n");
        let result = edit(&tool, "a", "x").await;
        match result {
            Err(ToolError::Precondition(msg)) => {
                assert!(msg.contains("or set replace_all"), "{msg}");
            }
            other => panic!("expected Precondition, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn identical_old_and_new_rejected() {
        let (dir, tool) = setup("content");
        let result = edit(&tool, "content", "content").await;
        assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
        assert_eq!(read(&dir), "content");
    }

    #[tokio::test]
    async fn empty_old_string_rejected() {
        let (_dir, tool) = setup("content");
        let result = edit(&tool, "", "x").await;
        assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
    }

    async fn edit_path(
        tool: &FileEdit,
        path: &str,
        old_string: &str,
        new_string: &str,
    ) -> Result<String, ToolError> {
        tool.execute(
            serde_json::json!({
                "path": path,
                "old_string": old_string,
                "new_string": new_string
            }),
            ToolCtx::default(),
        )
        .await
    }

    #[tokio::test]
    async fn third_identical_no_match_hard_stops() {
        let (_dir, tool) = setup("hello world");
        for _ in 0..2 {
            let result = edit(&tool, "missing", "x").await;
            assert!(
                matches!(result, Err(ToolError::Precondition(_))),
                "{result:?}"
            );
        }
        let result = edit(&tool, "missing", "x").await;
        match result {
            Err(ToolError::EditLoop {
                attempts: 3,
                outcome: EditFutility::FailedIdentically,
                ref path,
            }) => assert_eq!(path, "test.txt"),
            other => panic!("expected EditLoop, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn third_identical_no_change_hard_stops() {
        // Whitespace-flexible match where the replacement equals the
        // file's existing bytes: a no-change success.
        let (dir, tool) = setup("foo( 1 );\n");
        for _ in 0..2 {
            let result = edit(&tool, "foo(  1 );", "foo( 1 );").await.unwrap();
            assert!(result.contains("(no change)"), "{result}");
        }
        let result = edit(&tool, "foo(  1 );", "foo( 1 );").await;
        assert!(
            matches!(
                result,
                Err(ToolError::EditLoop {
                    attempts: 3,
                    outcome: EditFutility::NoChange,
                    ..
                })
            ),
            "{result:?}"
        );
        assert_eq!(read(&dir), "foo( 1 );\n");
    }

    #[tokio::test]
    async fn changing_edit_clears_futility() {
        let (_dir, tool) = setup("hello world");
        for _ in 0..2 {
            edit(&tool, "missing", "x").await.unwrap_err();
        }
        edit(&tool, "hello", "goodbye").await.unwrap();
        // The counter restarted: the same payload gets two more
        // ordinary errors before the guard would trip again.
        for _ in 0..2 {
            let result = edit(&tool, "missing", "x").await;
            assert!(
                matches!(result, Err(ToolError::Precondition(_))),
                "{result:?}"
            );
        }
    }

    #[tokio::test]
    async fn interleaved_failing_payloads_each_trip() {
        let (_dir, tool) = setup("hello world");
        for _ in 0..2 {
            edit(&tool, "missing a", "x").await.unwrap_err();
            edit(&tool, "missing b", "x").await.unwrap_err();
        }
        let result = edit(&tool, "missing a", "x").await;
        assert!(
            matches!(result, Err(ToolError::EditLoop { .. })),
            "{result:?}"
        );
        let result = edit(&tool, "missing b", "x").await;
        assert!(
            matches!(result, Err(ToolError::EditLoop { .. })),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn paths_do_not_share_futility() {
        let (dir, tool) = setup("hello world");
        std::fs::write(dir.path().join("other.txt"), "hello world").unwrap();
        for _ in 0..2 {
            edit(&tool, "missing", "x").await.unwrap_err();
        }
        let result = edit_path(&tool, "other.txt", "missing", "x").await;
        assert!(
            matches!(result, Err(ToolError::Precondition(_))),
            "{result:?}"
        );
    }

    #[test]
    fn history_window_evicts_old_fingerprints() {
        let mut history = EditHistory::default();
        assert_eq!(history.record_futile("p", 0), 1);
        for f in 1..=RECENT_WINDOW as u64 {
            history.record_futile("p", f);
        }
        // Fingerprint 0 was pushed out of the window.
        assert_eq!(history.record_futile("p", 0), 1);
    }

    #[test]
    fn history_path_cap_resets() {
        let mut history = EditHistory::default();
        for p in 0..MAX_PATHS {
            history.record_futile(&format!("p{p}"), 7);
        }
        // The overflowing path wipes the map; counts restart at one.
        assert_eq!(history.record_futile("overflow", 7), 1);
        assert_eq!(history.record_futile("p0", 7), 1);
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
