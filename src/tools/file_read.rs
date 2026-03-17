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

use super::Tool;
use super::path::PathGuard;
use crate::error::ToolError;

/// 10 MB — reject files larger than this to avoid flooding context.
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

/// Default number of lines returned when no limit is specified.
const DEFAULT_LIMIT: u32 = 2000;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// File path relative to the workspace.
    path: String,
    /// Start line (1-based). Defaults to 1.
    offset: Option<u32>,
    /// Maximum number of lines to return. Defaults to 2000.
    limit: Option<u32>,
}

/// Tool that reads file contents from the workspace.
pub struct FileRead {
    guard: PathGuard,
}

impl FileRead {
    pub fn new(guard: PathGuard) -> Self {
        Self { guard }
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
        serde_json::to_value(schemars::schema_for!(Args)).expect("schema serialization failed")
    }

    fn execute(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let args: Args = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

            let resolved = self.guard.resolve(&args.path)?;
            debug!(path = %args.path, "Reading file");

            let meta = std::fs::metadata(&resolved)
                .map_err(|e| ToolError::ExecutionFailed(format!("{}: {e}", args.path)))?;

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

            let content = std::fs::read_to_string(&resolved)
                .map_err(|e| ToolError::ExecutionFailed(format!("{}: {e}", args.path)))?;

            let total_lines = content.lines().count();
            let offset = args.offset.unwrap_or(1).max(1) as usize;
            let limit = args.limit.unwrap_or(DEFAULT_LIMIT) as usize;

            let (output, shown) = content
                .lines()
                .enumerate()
                .skip(offset.saturating_sub(1))
                .take(limit)
                .fold((String::new(), 0usize), |(mut acc, count), (i, line)| {
                    let line_num = i + 1;
                    let _ = writeln!(acc, "{line_num}\t{line}");
                    (acc, count + 1)
                });

            let output = format!(
                "{output}\n({shown} lines shown, {total_lines} total, {} bytes)",
                meta.len()
            );

            Ok(output)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(content: &str) -> (tempfile::TempDir, FileRead) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.txt"), content).unwrap();
        let guard = PathGuard::new(dir.path());
        (dir, FileRead::new(guard))
    }

    #[tokio::test]
    async fn read_entire_file() {
        let (_dir, tool) = setup("line1\nline2\nline3\n");
        let result = tool
            .execute(serde_json::json!({"path": "test.txt"}))
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
            .execute(serde_json::json!({"path": "test.txt", "offset": 3}))
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
            .execute(serde_json::json!({"path": "test.txt", "limit": 2}))
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
            .execute(serde_json::json!({"path": "test.txt", "offset": 2, "limit": 2}))
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
        let tool = FileRead::new(PathGuard::new(dir.path()));
        let result = tool
            .execute(serde_json::json!({"path": "missing.txt"}))
            .await;
        assert!(matches!(result, Err(ToolError::ExecutionFailed(_))));
    }

    #[tokio::test]
    async fn path_traversal_blocked() {
        let (_dir, tool) = setup("secret");
        let result = tool
            .execute(serde_json::json!({"path": "../etc/passwd"}))
            .await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn empty_file() {
        let (_dir, tool) = setup("");
        let result = tool
            .execute(serde_json::json!({"path": "test.txt"}))
            .await
            .unwrap();
        assert!(result.contains("0 lines shown"));
    }

    #[tokio::test]
    async fn large_file_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.txt");
        // Create a file just over the limit using sparse writes
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_FILE_SIZE + 1).unwrap();

        let tool = FileRead::new(PathGuard::new(dir.path()));
        let result = tool.execute(serde_json::json!({"path": "big.txt"})).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }
}
