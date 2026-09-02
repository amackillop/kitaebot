//! File reading tool.
//!
//! Reads a file from the workspace with optional line offset and limit.
//! Output includes line numbers for LLM context.

use std::fmt::Write;
use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;
use tracing::{debug, warn};

use super::path::PathGuard;
use super::{Tool, ToolCtx, string_or_value};
use crate::error::ToolError;

/// 10 MB — reject files larger than this to avoid flooding context.
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

/// Default number of lines returned when no limit is specified.
const DEFAULT_LIMIT: u32 = 2000;

/// Tokens reserved for the `<tool_output>` wrapper and the trailer so
/// a clamped result stays under the engine's inline threshold whole.
const CLAMP_RESERVE_TOKENS: usize = 64;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// File path relative to the workspace.
    path: String,
    /// Start line (1-based). Defaults to 1.
    #[serde(default, deserialize_with = "string_or_value")]
    offset: Option<u32>,
    /// Maximum number of lines to return. Defaults to 2000.
    #[serde(default, deserialize_with = "string_or_value")]
    limit: Option<u32>,
}

/// Tool that reads file contents from the workspace.
pub struct FileRead {
    guard: PathGuard,
    /// The engine's inline threshold (`context.tool_output_tokens`):
    /// output at or below it reaches the model whole instead of as an
    /// externalized `<file>` reference.
    inline_tokens: usize,
}

impl FileRead {
    pub fn new(guard: PathGuard, inline_tokens: u32) -> Self {
        Self {
            guard,
            inline_tokens: inline_tokens as usize,
        }
    }
}

impl Tool for FileRead {
    fn name(&self) -> &'static str {
        "file_read"
    }

    fn description(&self) -> &'static str {
        "Read a file from the workspace with optional line offset and limit"
    }

    fn parameters(&self) -> serde_json::Value {
        crate::tools::schema_of::<Args>()
    }

    fn execute(
        &self,
        args: serde_json::Value,
        ctx: ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        Box::pin(async move {
            // Sub-agent engines run a higher inline cap (spec 19); the
            // clamp must match the engine that will judge the result.
            let inline_tokens = ctx.tool_output_tokens.unwrap_or(self.inline_tokens);
            let args: Args = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

            let resolved = self.guard.resolve(&args.path)?;
            debug!(path = %args.path, "Reading file");

            let meta = std::fs::metadata(&resolved).map_err(|e| ToolError::Io {
                operation: "read",
                path: (&args.path).into(),
                source: e,
            })?;

            if meta.len() > MAX_FILE_SIZE {
                warn!(path = %args.path, size = meta.len(), "File too large");
                return Err(ToolError::Blocked {
                    operation: args.path,
                    guidance: format!(
                        "file too large: {} bytes (max {})",
                        meta.len(),
                        MAX_FILE_SIZE,
                    ),
                });
            }

            let content = std::fs::read_to_string(&resolved).map_err(|e| ToolError::Io {
                operation: "read",
                path: (&args.path).into(),
                source: e,
            })?;

            let total_lines = content.lines().count();
            let offset = args.offset.unwrap_or(1).max(1) as usize;
            let limit = args.limit.unwrap_or(DEFAULT_LIMIT) as usize;

            let window: Vec<(usize, &str)> = content
                .lines()
                .enumerate()
                .skip(offset.saturating_sub(1))
                .take(limit)
                .map(|(i, line)| (i + 1, line))
                .collect();

            let budget_bytes = inline_tokens
                .saturating_sub(CLAMP_RESERVE_TOKENS)
                .saturating_mul(4);
            let mut used = 0usize;
            let fitting = window
                .iter()
                .take_while(|(num, line)| {
                    // Numbered-line cost: digits + tab + content + newline.
                    used += num.ilog10() as usize + 1 + 1 + line.len() + 1;
                    used <= budget_bytes
                })
                .count();

            // A first line alone over budget cannot be windowed; fall
            // through whole and let the engine externalize it.
            let clamped = fitting < window.len() && fitting > 0;
            let shown = if clamped { fitting } else { window.len() };

            let output =
                window
                    .iter()
                    .take(shown)
                    .fold(String::new(), |mut acc, (line_num, line)| {
                        let _ = writeln!(acc, "{line_num}\t{line}");
                        acc
                    });

            let trailer = if clamped {
                let next = offset + shown;
                format!(
                    "({shown} lines shown, {total_lines} total, {} bytes; continue with offset={next})",
                    meta.len()
                )
            } else {
                format!(
                    "({shown} lines shown, {total_lines} total, {} bytes)",
                    meta.len()
                )
            };

            Ok(format!("{output}\n{trailer}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(content: &str) -> (tempfile::TempDir, FileRead) {
        setup_with_threshold(content, 4096)
    }

    fn setup_with_threshold(content: &str, inline_tokens: u32) -> (tempfile::TempDir, FileRead) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.txt"), content).unwrap();
        let guard = PathGuard::new(dir.path());
        (dir, FileRead::new(guard, inline_tokens))
    }

    #[tokio::test]
    async fn read_entire_file() {
        let (_dir, tool) = setup("line1\nline2\nline3\n");
        let result = tool
            .execute(serde_json::json!({"path": "test.txt"}), ToolCtx::default())
            .await
            .unwrap();
        assert!(result.contains("1\tline1"));
        assert!(result.contains("2\tline2"));
        assert!(result.contains("3\tline3"));
        assert!(result.contains("3 lines shown"));
    }

    #[tokio::test]
    async fn read_with_offset() {
        let (_dir, tool) = setup("a\nb\nc\nd\n");
        let result = tool
            .execute(
                serde_json::json!({"path": "test.txt", "offset": 3}),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert!(!result.contains("1\ta"));
        assert!(!result.contains("2\tb"));
        assert!(result.contains("3\tc"));
        assert!(result.contains("4\td"));
        assert!(result.contains("2 lines shown"));
    }

    #[tokio::test]
    async fn read_with_limit() {
        let (_dir, tool) = setup("a\nb\nc\nd\n");
        let result = tool
            .execute(
                serde_json::json!({"path": "test.txt", "limit": 2}),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert!(result.contains("1\ta"));
        assert!(result.contains("2\tb"));
        assert!(!result.contains("3\tc"));
        assert!(result.contains("2 lines shown"));
    }

    #[tokio::test]
    async fn read_with_offset_and_limit() {
        let (_dir, tool) = setup("a\nb\nc\nd\ne\n");
        let result = tool
            .execute(
                serde_json::json!({"path": "test.txt", "offset": 2, "limit": 2}),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert!(!result.contains("1\ta"));
        assert!(result.contains("2\tb"));
        assert!(result.contains("3\tc"));
        assert!(!result.contains("4\td"));
        assert!(result.contains("2 lines shown"));
    }

    #[tokio::test]
    async fn read_with_string_encoded_offset_and_limit() {
        let (_dir, tool) = setup("a\nb\nc\nd\ne\n");
        let result = tool
            .execute(
                serde_json::json!({"path": "test.txt", "offset": "2", "limit": "2"}),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert!(!result.contains("1\ta"));
        assert!(result.contains("2\tb"));
        assert!(result.contains("3\tc"));
        assert!(!result.contains("4\td"));
        assert!(result.contains("2 lines shown"));
    }

    #[tokio::test]
    async fn file_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let tool = FileRead::new(PathGuard::new(dir.path()), 4096);
        let result = tool
            .execute(
                serde_json::json!({"path": "missing.txt"}),
                ToolCtx::default(),
            )
            .await;
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::Io { .. }));
        // The model reads this: it must name the path and the reason.
        let msg = err.to_string();
        assert!(msg.contains("missing.txt"), "{msg}");
        assert!(msg.contains("No such file"), "{msg}");
    }

    #[tokio::test]
    async fn path_traversal_blocked() {
        let (_dir, tool) = setup("secret");
        let result = tool
            .execute(
                serde_json::json!({"path": "../etc/passwd"}),
                ToolCtx::default(),
            )
            .await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn empty_file() {
        let (_dir, tool) = setup("");
        let result = tool
            .execute(serde_json::json!({"path": "test.txt"}), ToolCtx::default())
            .await
            .unwrap();
        assert!(result.contains("0 lines shown"));
    }

    /// Issue #148: an oversized read must return usable text plus the
    /// next offset, not an externalized reference.
    #[tokio::test]
    async fn oversized_read_clamps_to_inline_window_with_continuation() {
        let content = "0123456789\n".repeat(100);
        let threshold = u32::try_from(CLAMP_RESERVE_TOKENS).unwrap() + 50;
        let (_dir, tool) = setup_with_threshold(&content, threshold);
        let result = tool
            .execute(serde_json::json!({"path": "test.txt"}), ToolCtx::default())
            .await
            .unwrap();

        assert!(
            crate::types::estimate_tokens(&result) <= threshold as usize,
            "clamped output must fit the inline threshold: {} tokens",
            crate::types::estimate_tokens(&result)
        );
        assert!(result.contains("100 total"), "{result}");
        let (_, tail) = result.rsplit_once("; continue with offset=").unwrap();
        let next: usize = tail.trim_end_matches(')').trim().parse().unwrap();

        let cont = tool
            .execute(
                serde_json::json!({"path": "test.txt", "offset": next}),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert!(
            cont.lines()
                .next()
                .unwrap()
                .starts_with(&format!("{next}\t")),
            "continuation must resume at the advertised offset: {cont}"
        );
    }

    /// Sub-agent contexts carry their own higher inline cap; the same
    /// read that clamps for the root must pass whole for them.
    #[tokio::test]
    async fn ctx_threshold_override_lifts_the_clamp() {
        let content = "0123456789\n".repeat(100);
        let threshold = u32::try_from(CLAMP_RESERVE_TOKENS).unwrap() + 50;
        let (_dir, tool) = setup_with_threshold(&content, threshold);
        let ctx = ToolCtx {
            tool_output_tokens: Some(20_000),
            ..ToolCtx::default()
        };
        let result = tool
            .execute(serde_json::json!({"path": "test.txt"}), ctx)
            .await
            .unwrap();
        assert!(result.contains("100 lines shown, 100 total"), "{result}");
        assert!(!result.contains("continue with offset"), "{result}");
    }

    /// A single line over budget cannot be windowed by lines; the read
    /// falls through whole so the engine externalizes it as before.
    #[tokio::test]
    async fn unwindowable_line_falls_through_without_continuation() {
        let content = "x".repeat(4000);
        let threshold = u32::try_from(CLAMP_RESERVE_TOKENS).unwrap() + 50;
        let (_dir, tool) = setup_with_threshold(&content, threshold);
        let result = tool
            .execute(serde_json::json!({"path": "test.txt"}), ToolCtx::default())
            .await
            .unwrap();
        assert!(result.contains(&"x".repeat(4000)), "{result}");
        assert!(!result.contains("continue with offset"), "{result}");
        assert!(result.contains("1 lines shown, 1 total"), "{result}");
    }

    #[tokio::test]
    async fn large_file_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.txt");
        // Create a file just over the limit using sparse writes
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_FILE_SIZE + 1).unwrap();

        let tool = FileRead::new(PathGuard::new(dir.path()), 4096);
        let result = tool
            .execute(serde_json::json!({"path": "big.txt"}), ToolCtx::default())
            .await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }
}
