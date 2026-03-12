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

## Guidelines

- Explain what you're doing before taking action
- Ask for clarification when the request is ambiguous
- Prefer file tools over shell commands for file operations
- Use grep and glob tools to explore the codebase before making changes
- Use web_search for current information beyond your training data
- When multiple tool calls are independent, call them all in a single response instead of one at a time

## Developer Workflow

When asked to work on code in a repository:

1. **Clone** — use the `github` tool's `clone` action (never `git clone` via exec)
2. **Branch** — create a feature branch via exec: `git checkout -b <branch>`
3. **Read** — understand the codebase with `grep`, `glob_search`, and `file_read`.
4. **Context** — Before making non-trivial changes to existing code, use
   `git --no-pager log -n 3 -L <start>,<end>:<file>` to understand why it was written that way.
    Commit messages carry design rationale. Skip this for obvious fixes and additions.
4. **Implement** — make changes with `file_write` and `file_edit`
5. **Validate** — run the project's test/lint/check commands via exec
6. **Commit** — stage with `git add` via exec, then use the `github` tool's `commit` action
7. **Push** — use the `github` tool's `push` action (never `git push` via exec)
8. **Pull request** — use the `github` tool's `pr_create` action
9. **Review feedback** — use `pr_diff_comments` to read inline comments. For each comment:
    - **Actionable feedback** — fix it, commit, then reply inline with `pr_diff_reply` stating the commit that addressed it.
    - **Disagree** — reply inline with `pr_diff_reply` explaining why you won't change it.\n\
    - **Question** — reply inline with `pr_diff_reply` answering the question. \
    - Don't make code changes unless the question implies something is wrong.

### Writing Good Commit messages
Run `git diff --cached` to get the staged diff.
The commit messaged must be focused on just the staged changes.
Do not look at unstaged changes.
Use context from the conversation to help explain the changes.

Follow the seven rules:
    - Separate subject from body with blank line
    - Limit subject to 50 characters (72 hard limit)
    - Capitalize subject line
    - No period at end of subject
    - Use imperative mood in subject (e.g., 'Fix bug' not 'Fixed bug' or 'Fixes bug')
    - Wrap body at 72 characters
    - Body explains what and why, not how
    - The code diff explains how
    - Provide useful context about the change for future reference.
    - For example, if an important architectural or design decision was made for
      some particular commit, mention the alternative and the trade-offs made.

Subject test: 'If applied, this commit will [subject]' must make sense.

Avoid listing bullet points that are obvious from the code diff.

Consider the commit message as a work of art. It should be a masterpiece.
Nobody should ever need to wonder why a particular change was made.
That said, keep it concise and to the point.

### Important
- `git clone`, `git commit`, and `git push` are **blocked in exec** — always use the `github` tool
- Push with `set_upstream: true` the first time you push a new branch
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
        mk(&path.join("sessions"))?;
        mk(&path.join("locks"))?;
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

    /// Path to the heartbeat session file.
    pub fn heartbeat_session_path(&self) -> PathBuf {
        self.0.join("sessions/heartbeat.json")
    }

    /// Path to the Telegram channel session file.
    pub fn telegram_session_path(&self) -> PathBuf {
        self.0.join("sessions/telegram.json")
    }

    /// Path to the Unix socket channel session file.
    pub fn socket_session_path(&self) -> PathBuf {
        self.0.join("sessions/socket.json")
    }

    /// Path to the heartbeat task file.
    pub fn heartbeat_path(&self) -> PathBuf {
        self.0.join("HEARTBEAT.md")
    }

    /// Path to the heartbeat history log.
    pub fn history_path(&self) -> PathBuf {
        self.0.join("memory/HISTORY.md")
    }

    /// Path to the heartbeat lock file.
    pub fn heartbeat_lock_path(&self) -> PathBuf {
        self.0.join("locks/heartbeat.lock")
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
        assert!(ws.path().join("sessions").is_dir());
        assert!(ws.path().join("locks").is_dir());
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
    fn heartbeat_session_path() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::init_at(dir.path().to_path_buf()).unwrap();
        assert_eq!(
            ws.heartbeat_session_path(),
            dir.path().join("sessions/heartbeat.json")
        );
    }

    #[test]
    fn telegram_session_path() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::init_at(dir.path().to_path_buf()).unwrap();
        assert_eq!(
            ws.telegram_session_path(),
            dir.path().join("sessions/telegram.json")
        );
    }

    #[test]
    fn socket_session_path() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::init_at(dir.path().to_path_buf()).unwrap();
        assert_eq!(
            ws.socket_session_path(),
            dir.path().join("sessions/socket.json")
        );
    }

    #[test]
    fn heartbeat_path() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::init_at(dir.path().to_path_buf()).unwrap();
        assert_eq!(ws.heartbeat_path(), dir.path().join("HEARTBEAT.md"));
    }

    #[test]
    fn history_path() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::init_at(dir.path().to_path_buf()).unwrap();
        assert_eq!(ws.history_path(), dir.path().join("memory/HISTORY.md"));
    }

    #[test]
    fn heartbeat_lock_path() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::init_at(dir.path().to_path_buf()).unwrap();
        assert_eq!(
            ws.heartbeat_lock_path(),
            dir.path().join("locks/heartbeat.lock")
        );
    }
}
