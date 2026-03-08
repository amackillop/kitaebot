//! GitHub integration tool.
//!
//! Provides authenticated git and GitHub CLI operations. The token never
//! reaches the exec tool — it is injected only into subprocesses spawned
//! by this module via `GIT_ASKPASS` (for git) or `GH_TOKEN` (for `gh`).
//!
//! # Token injection
//!
//! For `git clone`/`push`, a temporary helper script is written to a
//! private directory, set as `GIT_ASKPASS`, and deleted immediately after
//! the subprocess exits. The script prints the token to stdout when
//! invoked by git. The token is on disk for the duration of one git
//! command only.

use std::fmt::Write;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::Deserialize;
use tokio::process::Command;
use tokio::time::{Duration, timeout};
use tracing::debug;

use std::future::Future;
use std::pin::Pin;

use super::Tool;
use crate::error::ToolError;
use crate::secrets::Secret;

/// Maximum output bytes before truncation.
const MAX_OUTPUT_BYTES: usize = 10 * 1024;

/// Default timeout for git/gh operations.
const TIMEOUT_SECS: u64 = 120;

/// Arguments for the GitHub tool.
///
/// Each variant maps to one git/gh subcommand. Tagged with `action`
/// so the LLM produces `{"action": "clone", "url": "..."}`.
#[derive(Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Args {
    /// Clone a repository into the workspace.
    Clone {
        /// Repository URL (HTTPS or SSH). SSH URLs are rewritten to HTTPS
        /// automatically.
        url: String,
        /// Target directory name inside `projects/`. Defaults to the
        /// repository name derived from the URL.
        name: Option<String>,
    },
}

/// Authenticated GitHub operations.
pub struct GitHub {
    workspace_root: PathBuf,
    token: Secret,
    git_config: Option<PathBuf>,
    #[allow(dead_code)] // Used by later commits (PrCreate, commit trailers)
    co_authors: Vec<String>,
}

impl GitHub {
    pub fn new(
        workspace_root: impl Into<PathBuf>,
        token: Secret,
        git_config: Option<PathBuf>,
        co_authors: Vec<String>,
    ) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            token,
            git_config,
            co_authors,
        }
    }
}

impl Tool for GitHub {
    fn name(&self) -> &'static str {
        "github"
    }

    fn description(&self) -> &'static str {
        "Authenticated GitHub operations (clone, push, pull requests)"
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

            match args {
                Args::Clone { url, name } => self.clone_repo(&url, name.as_deref()).await,
            }
        })
    }
}

impl GitHub {
    /// Clone a repository into `projects/<name>`.
    async fn clone_repo(&self, url: &str, name: Option<&str>) -> Result<String, ToolError> {
        let https_url = to_https_url(url)?;
        let repo_name = match name {
            Some(n) => validate_name(n)?.to_string(),
            None => extract_repo_name(&https_url)?,
        };

        let projects_dir = self.workspace_root.join("projects");
        let target = projects_dir.join(&repo_name);

        if target.exists() {
            return Err(ToolError::ExecutionFailed(format!(
                "projects/{repo_name} already exists"
            )));
        }

        // Ensure projects/ exists.
        tokio::fs::create_dir_all(&projects_dir)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("mkdir projects/: {e}")))?;

        let askpass = AskPass::create(&self.token).await?;

        let mut cmd = Command::new("git");
        cmd.args(["clone", "--", &https_url, &repo_name])
            .current_dir(&projects_dir)
            .env_clear()
            .envs(super::safe_env())
            .env("GIT_ASKPASS", askpass.path())
            .env("GIT_TERMINAL_PROMPT", "0");

        if let Some(ref path) = self.git_config {
            cmd.env("GIT_CONFIG_GLOBAL", path);
        }

        debug!(url = %https_url, target = %target.display(), "Cloning repository");

        let output = timeout(Duration::from_secs(TIMEOUT_SECS), cmd.output())
            .await
            .map_err(|_| ToolError::Timeout)?
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        // askpass is dropped here — TempDir removes the token from disk.
        drop(askpass);

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let mut result = format!("$ git clone {https_url} projects/{repo_name}\n");

        if !stdout.is_empty() {
            result.push_str(&super::truncate_output(&stdout, MAX_OUTPUT_BYTES));
        }
        if !stderr.is_empty() {
            if !stdout.is_empty() {
                result.push('\n');
            }
            result.push_str(&super::truncate_output(&stderr, MAX_OUTPUT_BYTES));
        }

        let _ = write!(
            result,
            "\nExit code: {}",
            output.status.code().unwrap_or(-1)
        );

        if !output.status.success() {
            return Err(ToolError::ExecutionFailed(result));
        }

        Ok(result)
    }
}

// ── GIT_ASKPASS helper ──────────────────────────────────────────────

/// A temporary `GIT_ASKPASS` script that prints the token.
///
/// The script lives in a private temp directory (mode 0700). The
/// directory is owned by a `TempDir` and removed on drop, so cleanup
/// happens even if the git command fails or the future is cancelled.
struct AskPass {
    /// Path to the executable script inside `_dir`.
    path: PathBuf,
    /// Owns the temp directory. Removed on drop.
    _dir: tempfile::TempDir,
}

impl AskPass {
    async fn create(token: &Secret) -> Result<Self, ToolError> {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::Builder::new()
            .prefix("kitaebot-askpass-")
            .tempdir()
            .map_err(|e| ToolError::ExecutionFailed(format!("tmpdir: {e}")))?;

        let path = dir.path().join("askpass");
        let script = format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", token.expose());

        tokio::fs::write(&path, &script)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("write askpass: {e}")))?;

        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("chmod askpass: {e}")))?;

        Ok(Self { path, _dir: dir })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

// ── URL handling ────────────────────────────────────────────────────

/// Convert SSH-style URLs to HTTPS. Passes HTTPS URLs through unchanged.
///
/// Handles:
/// - `git@github.com:owner/repo.git` → `https://github.com/owner/repo.git`
/// - `ssh://git@github.com/owner/repo.git` → `https://github.com/owner/repo.git`
/// - `https://github.com/owner/repo.git` → unchanged
fn to_https_url(url: &str) -> Result<String, ToolError> {
    // Already HTTPS
    if url.starts_with("https://") {
        return Ok(url.to_string());
    }

    // SCP-style: git@github.com:owner/repo.git
    if let Some(rest) = url.strip_prefix("git@")
        && let Some((host, path)) = rest.split_once(':')
    {
        return Ok(format!("https://{host}/{path}"));
    }

    // ssh://git@github.com/owner/repo.git
    if let Some(rest) = url.strip_prefix("ssh://git@") {
        return Ok(format!("https://{rest}"));
    }

    Err(ToolError::InvalidArguments(format!(
        "unsupported URL scheme: {url}"
    )))
}

/// Extract the repository name from an HTTPS URL.
///
/// `https://github.com/owner/repo.git` → `repo`
/// `https://github.com/owner/repo` → `repo`
fn extract_repo_name(url: &str) -> Result<String, ToolError> {
    let path = url
        .strip_prefix("https://")
        .unwrap_or(url)
        .trim_end_matches('/')
        .trim_end_matches(".git");

    path.rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(String::from)
        .ok_or_else(|| ToolError::InvalidArguments(format!("cannot extract repo name from: {url}")))
}

/// Validate a user-provided directory name.
///
/// Rejects path traversal, absolute paths, and slashes.
fn validate_name(name: &str) -> Result<&str, ToolError> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || name.starts_with('.')
        || name.starts_with('-')
    {
        return Err(ToolError::InvalidArguments(format!(
            "invalid directory name: {name}"
        )));
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── URL conversion ──────────────────────────────────────────────

    #[test]
    fn https_url_passthrough() {
        let url = "https://github.com/owner/repo.git";
        assert_eq!(to_https_url(url).unwrap(), url);
    }

    #[test]
    fn scp_style_to_https() {
        assert_eq!(
            to_https_url("git@github.com:owner/repo.git").unwrap(),
            "https://github.com/owner/repo.git"
        );
    }

    #[test]
    fn ssh_url_to_https() {
        assert_eq!(
            to_https_url("ssh://git@github.com/owner/repo.git").unwrap(),
            "https://github.com/owner/repo.git"
        );
    }

    #[test]
    fn unsupported_scheme_rejected() {
        assert!(to_https_url("ftp://example.com/repo").is_err());
    }

    // ── Repo name extraction ────────────────────────────────────────

    #[test]
    fn extract_name_with_git_suffix() {
        assert_eq!(
            extract_repo_name("https://github.com/owner/repo.git").unwrap(),
            "repo"
        );
    }

    #[test]
    fn extract_name_without_git_suffix() {
        assert_eq!(
            extract_repo_name("https://github.com/owner/repo").unwrap(),
            "repo"
        );
    }

    #[test]
    fn extract_name_trailing_slash() {
        assert_eq!(
            extract_repo_name("https://github.com/owner/repo/").unwrap(),
            "repo"
        );
    }

    // ── Name validation ─────────────────────────────────────────────

    #[test]
    fn valid_name() {
        assert_eq!(validate_name("myrepo").unwrap(), "myrepo");
        assert_eq!(validate_name("my_repo").unwrap(), "my_repo");
        assert_eq!(validate_name("my-repo").unwrap(), "my-repo");
    }

    #[test]
    fn reject_traversal() {
        assert!(validate_name("..").is_err());
        assert!(validate_name("../escape").is_err());
    }

    #[test]
    fn reject_slashes() {
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("a\\b").is_err());
    }

    #[test]
    fn reject_hidden() {
        assert!(validate_name(".hidden").is_err());
    }

    #[test]
    fn reject_dash_prefix() {
        assert!(validate_name("-flag").is_err());
    }

    #[test]
    fn reject_empty() {
        assert!(validate_name("").is_err());
    }

    // ── Schema ──────────────────────────────────────────────────────

    #[test]
    fn schema_requires_action() {
        let schema = serde_json::to_value(schemars::schema_for!(Args)).unwrap();
        // Tagged enum — should have oneOf or use discriminator
        assert!(
            schema.to_string().contains("action"),
            "schema must include action discriminator: {schema}"
        );
    }

    #[test]
    fn deserialize_clone_args() {
        let json = serde_json::json!({
            "action": "clone",
            "url": "https://github.com/owner/repo.git"
        });
        let args: Args = serde_json::from_value(json).unwrap();
        assert!(
            matches!(args, Args::Clone { url, name } if url.contains("owner/repo") && name.is_none())
        );
    }

    #[test]
    fn deserialize_clone_with_name() {
        let json = serde_json::json!({
            "action": "clone",
            "url": "https://github.com/owner/repo.git",
            "name": "custom"
        });
        let args: Args = serde_json::from_value(json).unwrap();
        assert!(matches!(args, Args::Clone { name: Some(n), .. } if n == "custom"));
    }
}
