//! Workspace-confined path resolution.
//!
//! `PathGuard` ensures all tool file operations stay within the workspace root.
//! Rejects null bytes, `../` traversal, and absolute paths. Canonicalizes the
//! result and verifies it is under the workspace.

use std::path::{Path, PathBuf};

use crate::error::ToolError;

/// Resolves relative paths within a workspace, rejecting escapes.
#[derive(Clone)]
pub struct PathGuard {
    root: PathBuf,
}

impl PathGuard {
    /// Create a new guard rooted at `workspace`.
    ///
    /// Canonicalizes the root so later comparisons work even if the workspace
    /// path contained symlinks.
    pub fn new(workspace: &Path) -> Self {
        Self {
            root: workspace
                .canonicalize()
                .unwrap_or_else(|_| workspace.to_path_buf()),
        }
    }

    /// Resolve a relative path to an existing file within the workspace.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, ToolError> {
        let candidate = self.validate_and_join(path)?;
        let resolved = candidate
            .canonicalize()
            .map_err(|e| ToolError::ExecutionFailed(format!("{path}: {e}")))?;
        self.ensure_under_root(&resolved, path)
    }

    /// Resolve a relative path for a file that may not exist yet.
    ///
    /// Validates the path (no traversal, no absolute, no null bytes) and
    /// joins it to the workspace root. Does not require the parent to exist
    /// — callers are expected to `create_dir_all` as needed.
    pub fn resolve_new(&self, path: &str) -> Result<PathBuf, ToolError> {
        let candidate = self.validate_and_join(path)?;
        self.ensure_under_root(&candidate, path)
    }

    fn validate_and_join(&self, path: &str) -> Result<PathBuf, ToolError> {
        if path.contains('\0') {
            return Err(ToolError::Blocked("null byte in path".into()));
        }
        if path.contains("../") || path.contains("..\\") {
            return Err(ToolError::Blocked("path traversal detected".into()));
        }
        if path == ".." {
            return Err(ToolError::Blocked("path traversal detected".into()));
        }
        if Path::new(path).is_absolute() {
            return Err(ToolError::Blocked("absolute paths not allowed".into()));
        }
        Ok(self.root.join(path))
    }

    fn ensure_under_root(&self, resolved: &Path, original: &str) -> Result<PathBuf, ToolError> {
        if resolved.starts_with(&self.root) {
            Ok(resolved.to_path_buf())
        } else {
            Err(ToolError::Blocked(format!(
                "path escapes workspace: {original}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_simple_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("foo.txt");
        std::fs::write(&file, "hello").unwrap();

        let guard = PathGuard::new(dir.path());
        let resolved = guard.resolve("foo.txt").unwrap();
        assert_eq!(resolved, file.canonicalize().unwrap());
    }

    #[test]
    fn resolve_nested_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        let file = dir.path().join("a/b/c.txt");
        std::fs::write(&file, "nested").unwrap();

        let guard = PathGuard::new(dir.path());
        let resolved = guard.resolve("a/b/c.txt").unwrap();
        assert_eq!(resolved, file.canonicalize().unwrap());
    }

    #[test]
    fn reject_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let guard = PathGuard::new(dir.path());
        let err = guard.resolve("../etc/passwd").unwrap_err();
        assert!(matches!(err, ToolError::Blocked(_)));
    }

    #[test]
    fn reject_bare_dotdot() {
        let dir = tempfile::tempdir().unwrap();
        let guard = PathGuard::new(dir.path());
        let err = guard.resolve("..").unwrap_err();
        assert!(matches!(err, ToolError::Blocked(_)));
    }

    #[test]
    fn reject_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let guard = PathGuard::new(dir.path());
        let err = guard.resolve("/etc/passwd").unwrap_err();
        assert!(matches!(err, ToolError::Blocked(_)));
    }

    #[test]
    fn reject_null_byte() {
        let dir = tempfile::tempdir().unwrap();
        let guard = PathGuard::new(dir.path());
        let err = guard.resolve("foo\0bar").unwrap_err();
        assert!(matches!(err, ToolError::Blocked(_)));
    }

    #[test]
    fn resolve_nonexistent_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let guard = PathGuard::new(dir.path());
        let err = guard.resolve("missing.txt").unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed(_)));
    }

    #[test]
    fn resolve_new_in_existing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let guard = PathGuard::new(dir.path());
        let resolved = guard.resolve_new("new_file.txt").unwrap();
        assert!(resolved.starts_with(dir.path().canonicalize().unwrap()));
        assert!(resolved.ends_with("new_file.txt"));
    }

    #[test]
    fn resolve_new_in_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        let guard = PathGuard::new(dir.path());
        let resolved = guard.resolve_new("sub/new.txt").unwrap();
        assert!(resolved.starts_with(dir.path().canonicalize().unwrap()));
        assert!(resolved.ends_with("sub/new.txt"));
    }

    #[test]
    fn resolve_new_rejects_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let guard = PathGuard::new(dir.path());
        let err = guard.resolve_new("../escape.txt").unwrap_err();
        assert!(matches!(err, ToolError::Blocked(_)));
    }
}
