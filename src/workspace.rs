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

const DEFAULT_SOUL: &str = "\
# Soul

I am kitaebot, a personal AI assistant.

## Personality

- Helpful and direct
- Concise, not verbose
- Curious about the user's goals

## Values

- Accuracy over speed
- Privacy and security
- Transparency in actions

## Communication Style

- Be clear and specific
- Explain reasoning when helpful
- Ask clarifying questions when needed
- Don't use emojis unless the user does
";

const DEFAULT_AGENTS: &str = "\
# Agent Instructions

## Tools

You have access to:
- `exec` — Run shell commands in the workspace

## Guidelines

- Explain what you're doing before taking action
- Ask for clarification when the request is ambiguous
- All file operations happen via shell commands

## Memory

- Session is persisted in session.json
- Long-term facts go in memory/MEMORY.md (future)
";

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
    /// Creates the directory tree and writes default template files if they
    /// don't already exist.
    pub fn init_at(path: PathBuf) -> Result<Self, WorkspaceError> {
        let mk = |dir: &Path| {
            fs::create_dir_all(dir).map_err(|e| WorkspaceError::Init(dir.to_path_buf(), e))
        };

        mk(&path)?;
        mk(&path.join("memory"))?;
        mk(&path.join("projects"))?;

        let ws = Self(path);
        ws.ensure_template("SOUL.md", DEFAULT_SOUL)?;
        ws.ensure_template("AGENTS.md", DEFAULT_AGENTS)?;

        Ok(ws)
    }

    /// Root path of the workspace.
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Path to the session JSON file.
    pub fn session_path(&self) -> PathBuf {
        self.0.join("session.json")
    }

    /// Build the system prompt from workspace files.
    ///
    /// Reads `SOUL.md`, `AGENTS.md`, and optionally `USER.md`, concatenating
    /// them into a single system prompt. Missing optional files are silently
    /// skipped.
    pub fn system_prompt(&self) -> String {
        let mut prompt = String::new();

        for name in ["SOUL.md", "AGENTS.md", "USER.md"] {
            if let Ok(content) = fs::read_to_string(self.0.join(name)) {
                if !prompt.is_empty() {
                    prompt.push('\n');
                }
                prompt.push_str(&content);
            }
        }

        prompt
    }

    /// Write a template file only if it doesn't already exist.
    fn ensure_template(&self, name: &str, content: &str) -> Result<(), WorkspaceError> {
        use std::io::Write;

        let path = self.0.join(name);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => f
                .write_all(content.as_bytes())
                .map_err(|e| WorkspaceError::Init(path, e)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(WorkspaceError::Init(path, e)),
        }
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

        assert!(ws.path().join("SOUL.md").exists());
        assert!(ws.path().join("AGENTS.md").exists());
        assert!(ws.path().join("memory").is_dir());
        assert!(ws.path().join("projects").is_dir());
        assert!(!ws.path().join("USER.md").exists());
    }

    #[test]
    fn init_preserves_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let custom = "# Custom soul\n";
        fs::write(dir.path().join("SOUL.md"), custom).unwrap();

        let ws = Workspace::init_at(dir.path().to_path_buf()).unwrap();
        let content = fs::read_to_string(ws.path().join("SOUL.md")).unwrap();
        assert_eq!(content, custom);
    }

    #[test]
    fn system_prompt_concatenates_files() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::init_at(dir.path().to_path_buf()).unwrap();
        let prompt = ws.system_prompt();

        assert!(prompt.contains("# Soul"));
        assert!(prompt.contains("# Agent Instructions"));
    }

    #[test]
    fn system_prompt_includes_user_md() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::init_at(dir.path().to_path_buf()).unwrap();
        fs::write(ws.path().join("USER.md"), "# User Preferences\n").unwrap();

        let prompt = ws.system_prompt();
        assert!(prompt.contains("# User Preferences"));
    }

    #[test]
    fn session_path() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::init_at(dir.path().to_path_buf()).unwrap();
        assert_eq!(ws.session_path(), dir.path().join("session.json"));
    }
}
