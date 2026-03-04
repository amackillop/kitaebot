//! File writing tool.
//!
//! Writes content to a file in the workspace. Creates parent directories
//! as needed. Uses `PathGuard::resolve_new` so the file need not exist yet.

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
    /// Content to write to the file.
    content: String,
}

/// Tool that writes file contents in the workspace.
pub struct FileWrite {
    guard: PathGuard,
}

impl FileWrite {
    pub fn new(guard: PathGuard) -> Self {
        Self { guard }
    }
}

impl Tool for FileWrite {
    fn name(&self) -> &'static str {
        "file_write"
    }

    fn description(&self) -> &'static str {
        "Write content to a file in the workspace, creating parent directories as needed"
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

            let resolved = self.guard.resolve_new(&args.path)?;

            if let Some(parent) = resolved.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    ToolError::ExecutionFailed(format!("{}: {e}", parent.display()))
                })?;
            }

            let bytes = args.content.len();
            debug!(path = %args.path, bytes, "Writing file");
            std::fs::write(&resolved, &args.content)
                .map_err(|e| ToolError::ExecutionFailed(format!("{}: {e}", args.path)))?;

            Ok(format!("Wrote {bytes} bytes to {}", args.path))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let tool = FileWrite::new(PathGuard::new(dir.path()));
        let result = tool
            .execute(serde_json::json!({"path": "hello.txt", "content": "hello world"}))
            .await
            .unwrap();
        assert!(result.contains("11 bytes"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("hello.txt")).unwrap(),
            "hello world"
        );
    }

    #[tokio::test]
    async fn overwrite_existing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "old").unwrap();
        let tool = FileWrite::new(PathGuard::new(dir.path()));
        tool.execute(serde_json::json!({"path": "f.txt", "content": "new"}))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "new"
        );
    }

    #[tokio::test]
    async fn auto_create_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let tool = FileWrite::new(PathGuard::new(dir.path()));
        tool.execute(serde_json::json!({"path": "a/b/c.txt", "content": "deep"}))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a/b/c.txt")).unwrap(),
            "deep"
        );
    }

    #[tokio::test]
    async fn path_traversal_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let tool = FileWrite::new(PathGuard::new(dir.path()));
        let result = tool
            .execute(serde_json::json!({"path": "../escape.txt", "content": "bad"}))
            .await;
        assert!(matches!(result, Err(ToolError::Blocked(_))));
    }
}
