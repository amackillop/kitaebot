//! Glob file search tool.
//!
//! Finds files matching a glob pattern within the workspace.
//! Returns sorted relative paths, capped at 1000 results.

use std::fmt::Write;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;

use super::Tool;
use crate::error::ToolError;

/// Maximum number of results returned.
const MAX_RESULTS: usize = 1000;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Glob pattern relative to the workspace (e.g. `"**/*.rs"`).
    pattern: String,
}

/// Tool that finds files matching a glob pattern in the workspace.
pub struct GlobSearch {
    root: PathBuf,
}

impl GlobSearch {
    pub fn new(workspace: &Path) -> Self {
        Self {
            root: workspace
                .canonicalize()
                .unwrap_or_else(|_| workspace.to_path_buf()),
        }
    }
}

impl Tool for GlobSearch {
    fn name(&self) -> &'static str {
        "glob_search"
    }

    fn description(&self) -> &'static str {
        "Find files matching a glob pattern in the workspace"
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

            if args.pattern.contains("../") || args.pattern.contains("..\\") {
                return Err(ToolError::Blocked("path traversal in pattern".into()));
            }

            let full_pattern = self.root.join(&args.pattern);
            let pattern_str = full_pattern.to_str().ok_or_else(|| {
                ToolError::InvalidArguments("pattern contains invalid UTF-8".into())
            })?;

            let mut paths: Vec<PathBuf> = glob::glob(pattern_str)
                .map_err(|e| ToolError::InvalidArguments(format!("invalid glob pattern: {e}")))?
                .filter_map(Result::ok)
                .filter(|p| p.starts_with(&self.root))
                .take(MAX_RESULTS + 1)
                .collect();

            let truncated = paths.len() > MAX_RESULTS;
            paths.truncate(MAX_RESULTS);
            paths.sort();

            let output = paths.iter().fold(String::new(), |mut acc, p| {
                if let Ok(rel) = p.strip_prefix(&self.root) {
                    let _ = writeln!(acc, "{}", rel.display());
                }
                acc
            });

            let summary = if truncated {
                format!("\n({MAX_RESULTS}+ matches, results truncated)")
            } else {
                format!("\n({} matches)", paths.len())
            };

            Ok(format!("{output}{summary}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, GlobSearch) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/a")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "").unwrap();
        std::fs::write(dir.path().join("src/a/mod.rs"), "").unwrap();
        std::fs::write(dir.path().join("README.md"), "").unwrap();
        let tool = GlobSearch::new(dir.path());
        (dir, tool)
    }

    #[tokio::test]
    async fn match_rust_files() {
        let (_dir, tool) = setup();
        let result = tool
            .execute(serde_json::json!({"pattern": "**/*.rs"}))
            .await
            .unwrap();
        assert!(result.contains("src/main.rs"));
        assert!(result.contains("src/lib.rs"));
        assert!(result.contains("src/a/mod.rs"));
        assert!(!result.contains("README.md"));
        assert!(result.contains("3 matches"));
    }

    #[tokio::test]
    async fn match_specific_file() {
        let (_dir, tool) = setup();
        let result = tool
            .execute(serde_json::json!({"pattern": "README.md"}))
            .await
            .unwrap();
        assert!(result.contains("README.md"));
        assert!(result.contains("1 matches"));
    }

    #[tokio::test]
    async fn no_matches() {
        let (_dir, tool) = setup();
        let result = tool
            .execute(serde_json::json!({"pattern": "**/*.py"}))
            .await
            .unwrap();
        assert!(result.contains("0 matches"));
    }

    #[tokio::test]
    async fn traversal_rejected() {
        let (_dir, tool) = setup();
        let result = tool
            .execute(serde_json::json!({"pattern": "../**/*"}))
            .await;
        assert!(matches!(result, Err(ToolError::Blocked(_))));
    }

    #[tokio::test]
    async fn result_cap() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..MAX_RESULTS + 10 {
            std::fs::write(dir.path().join(format!("{i}.txt")), "").unwrap();
        }
        let tool = GlobSearch::new(dir.path());
        let result = tool
            .execute(serde_json::json!({"pattern": "*.txt"}))
            .await
            .unwrap();
        assert!(result.contains("truncated"));
    }
}
