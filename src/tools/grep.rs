//! Grep tool.
//!
//! Searches file contents for a regex pattern using the ripgrep crate.
//! No external binary required.

use std::fmt::Write;
use std::future::Future;
use std::pin::Pin;

use grep::regex::RegexMatcher;
use grep::searcher::Searcher;
use grep::searcher::sinks::UTF8;
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use schemars::JsonSchema;
use serde::Deserialize;

use super::Tool;
use super::path::PathGuard;
use crate::error::ToolError;

/// Max matches returned.
const MAX_MATCHES: usize = 200;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Regex pattern to search for.
    pattern: String,
    /// Directory to search in, relative to workspace. Defaults to `"."`.
    path: Option<String>,
    /// File glob filter (e.g. `"*.rs"`).
    include: Option<String>,
}

/// Tool that searches file contents for regex patterns.
pub struct Grep {
    guard: PathGuard,
}

impl Grep {
    pub fn new(guard: PathGuard) -> Self {
        Self { guard }
    }
}

impl Tool for Grep {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Search file contents for a regex pattern in the workspace"
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

            let search_dir = match &args.path {
                Some(p) => self.guard.resolve(p)?,
                None => self.guard.resolve(".")?,
            };

            let matcher = RegexMatcher::new(&args.pattern)
                .map_err(|e| ToolError::InvalidArguments(format!("invalid regex: {e}")))?;

            let mut walk = WalkBuilder::new(&search_dir);
            walk.hidden(false);

            if let Some(include) = &args.include {
                let mut overrides = OverrideBuilder::new(&search_dir);
                overrides
                    .add(include)
                    .map_err(|e| ToolError::InvalidArguments(format!("invalid glob: {e}")))?;
                walk.overrides(
                    overrides
                        .build()
                        .map_err(|e| ToolError::InvalidArguments(format!("invalid glob: {e}")))?,
                );
            }

            let (results, _) = walk
                .build()
                .filter_map(Result::ok)
                .filter(|e| e.file_type().is_some_and(|ft| ft.is_file()))
                .fold((String::new(), 0usize), |(mut acc, mut count), entry| {
                    if count >= MAX_MATCHES {
                        return (acc, count);
                    }
                    let path = entry.path();
                    let rel = path.strip_prefix(&search_dir).unwrap_or(path);
                    let _ = Searcher::new().search_path(
                        &matcher,
                        path,
                        UTF8(|line_num, line| {
                            if count >= MAX_MATCHES {
                                return Ok(false);
                            }
                            let _ = write!(acc, "{}:{line_num}:{line}", rel.display());
                            if !line.ends_with('\n') {
                                acc.push('\n');
                            }
                            count += 1;
                            Ok(true)
                        }),
                    );
                    (acc, count)
                });

            if results.is_empty() {
                Ok("No matches found.".to_string())
            } else {
                Ok(super::truncate_output(&results, 10 * 1024).into_owned())
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Grep) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn hello() {}\npub fn world() {}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("README.md"), "# Hello\n").unwrap();
        let tool = Grep::new(PathGuard::new(dir.path()));
        (dir, tool)
    }

    #[tokio::test]
    async fn pattern_match() {
        let (_dir, tool) = setup();
        let result = tool
            .execute(serde_json::json!({"pattern": "fn \\w+"}))
            .await
            .unwrap();
        assert!(result.contains("main"));
        assert!(result.contains("hello"));
    }

    #[tokio::test]
    async fn include_filter() {
        let (_dir, tool) = setup();
        let result = tool
            .execute(serde_json::json!({"pattern": "Hello", "include": "*.md"}))
            .await
            .unwrap();
        assert!(result.contains("Hello"));
        assert!(!result.contains("fn"));
    }

    #[tokio::test]
    async fn no_matches() {
        let (_dir, tool) = setup();
        let result = tool
            .execute(serde_json::json!({"pattern": "nonexistent_xyz"}))
            .await
            .unwrap();
        assert!(result.contains("No matches"));
    }

    #[tokio::test]
    async fn directory_traversal_blocked() {
        let (_dir, tool) = setup();
        let result = tool
            .execute(serde_json::json!({"pattern": "secret", "path": "../etc"}))
            .await;
        assert!(matches!(result, Err(ToolError::Blocked(_))));
    }
}
