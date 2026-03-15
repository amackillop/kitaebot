//! Workspace management.
//!
//! The workspace is the root directory where kitaebot stores its configuration,
//! session data, and project files. Resolved from `KITAEBOT_WORKSPACE` env var,
//! falling back to `~/.local/share/kitaebot` (XDG data home).

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::WorkspaceError;

const ENV_VAR: &str = "KITAEBOT_WORKSPACE";
const APP_NAME: &str = "kitaebot";
const SYSTEM_PROMPTS: [&str; 3] = ["SOUL.md", "AGENTS.md", "USER.md"];

/// An initialized workspace directory.
///
/// Construction via [`Workspace::init`] guarantees the directory exists
/// and contains the required structure.
pub struct Workspace(PathBuf);

impl Workspace {
    /// Initialize the workspace from `KITAEBOT_WORKSPACE` env var or XDG default.
    ///
    /// Fallback: `$XDG_DATA_HOME/kitaebot`, then `~/.local/share/kitaebot`.
    pub fn init() -> Result<Self, WorkspaceError> {
        let path = std::env::var(ENV_VAR)
            .map(PathBuf::from)
            .or_else(|_| default_data_dir())
            .map_err(|e| WorkspaceError::Init(PathBuf::from(APP_NAME), e))?;
        Self::init_at(path)
    }

    /// Initialize the workspace at an explicit path.
    ///
    /// Creates the directory tree. Prompt files (SOUL.md, AGENTS.md, etc.)
    /// are expected to be provisioned externally (e.g. via Nix tmpfiles).
    pub fn init_at(path: PathBuf) -> Result<Self, WorkspaceError> {
        let mk = |dir: &Path| {
            fs::create_dir_all(dir).map_err(|e| WorkspaceError::Init(dir.to_path_buf(), e))
        };

        mk(&path)?;
        mk(&path.join("sessions"))?;
        mk(&path.join("memory"))?;
        mk(&path.join("projects"))?;

        Ok(Self(path))
    }

    /// Root path of the workspace.
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Path to the unified agent session file.
    pub fn session_path(&self) -> PathBuf {
        self.0.join("sessions/session.json")
    }

    /// Path to the heartbeat task file.
    pub fn heartbeat_path(&self) -> PathBuf {
        self.0.join("HEARTBEAT.md")
    }

    /// Path to the heartbeat history log.
    pub fn history_path(&self) -> PathBuf {
        self.0.join("memory/HISTORY.md")
    }

    /// Path to the GitHub poll state file.
    pub fn github_poll_state_path(&self) -> PathBuf {
        self.0.join("memory/github_poll_state.json")
    }

    /// Build the system prompt from workspace files.
    ///
    /// Reads [`SYSTEM_PROMPTS`] concatenating
    /// them into a single system prompt. Missing files emit a warning.
    pub fn system_prompt(&self) -> String {
        let mut prompt = String::new();

        for name in SYSTEM_PROMPTS {
            match fs::read_to_string(self.0.join(name)) {
                Ok(content) => {
                    if !prompt.is_empty() {
                        prompt.push('\n');
                    }
                    prompt.push_str(&content);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    tracing::warn!("{name} not found in workspace");
                }
                Err(e) => tracing::warn!("failed to read {name}: {e}"),
            }
        }

        prompt
    }
}

/// Resolve the default data directory following XDG Base Directory spec.
fn default_data_dir() -> Result<PathBuf, std::io::Error> {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "neither XDG_DATA_HOME nor HOME is set",
            )
        })?;
    Ok(base.join(APP_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_creates_structure() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::init_at(dir.path().to_path_buf()).unwrap();

        assert!(ws.path().join("sessions").is_dir());
        assert!(ws.path().join("memory").is_dir());
        assert!(ws.path().join("projects").is_dir());
    }

    #[test]
    fn system_prompt_concatenates_all_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("SOUL.md"), "# Soul\n").unwrap();
        fs::write(dir.path().join("AGENTS.md"), "# Agents\n").unwrap();
        fs::write(dir.path().join("USER.md"), "# User Preferences\n").unwrap();
        let ws = Workspace::init_at(dir.path().to_path_buf()).unwrap();

        let prompt = ws.system_prompt();
        assert!(prompt.contains("# Soul"));
        assert!(prompt.contains("# Agents"));
        assert!(prompt.contains("# User Preferences"));
    }
}
