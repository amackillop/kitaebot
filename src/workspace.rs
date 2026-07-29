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

/// The agent's persona and workflow, embedded at build time so they
/// are versioned with the code that references their tools and cannot
/// go missing at runtime.
const PRODUCT_PROMPT: &str = concat!(
    include_str!("prompts/SOUL.md"),
    "\n",
    include_str!("prompts/AGENTS.md"),
);

/// Operator preferences, provisioned into the workspace. Optional.
const USER_PROMPT: &str = "USER.md";

/// An initialized workspace directory.
///
/// Construction via [`Workspace::init`] guarantees the directory exists
/// and contains the required structure.
pub struct Workspace {
    root: PathBuf,
    system_prompt: String,
}

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
    /// Creates the directory tree. The persona and workflow are compiled
    /// in; only USER.md is read from the workspace (provisioned via Nix).
    pub fn init_at(path: PathBuf) -> Result<Self, WorkspaceError> {
        let mk = |dir: &Path| {
            fs::create_dir_all(dir).map_err(|e| WorkspaceError::Init(dir.to_path_buf(), e))
        };

        mk(&path)?;
        mk(&path.join("sessions"))?;
        mk(&path.join("memory"))?;
        mk(&path.join("memory/topics"))?;
        mk(&path.join("projects"))?;
        mk(&path.join("state"))?;

        // HISTORY.md was at the root while it belonged to the heartbeat
        // (spec 21). The heartbeat is gone and the log is machine-owned,
        // so it lives under state/ now; move an existing one rather than
        // silently starting a second.
        let legacy_history = path.join("HISTORY.md");
        let history = path.join("state/HISTORY.md");
        if legacy_history.is_file()
            && !history.exists()
            && let Err(e) = fs::rename(&legacy_history, &history)
        {
            tracing::warn!("failed to move HISTORY.md into state/: {e}");
        }

        let system_prompt = read_system_prompt(&path);
        Ok(Self {
            root: path,
            system_prompt,
        })
    }

    /// Root path of the workspace.
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Directory holding per-session storage.
    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    /// Directory holding machine-owned runtime state (engine store,
    /// channel poll cursors).
    pub fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }

    /// Directory holding the memory subsystem's files (spec 21).
    pub fn memory_dir(&self) -> PathBuf {
        self.root.join("memory")
    }

    /// Path to the duty history log.
    pub fn history_path(&self) -> PathBuf {
        self.state_dir().join("HISTORY.md")
    }

    /// Path to the notification mirror: every message the Notifier
    /// sends, greppable without Telegram.
    pub fn notifications_path(&self) -> PathBuf {
        self.state_dir().join("NOTIFICATIONS.md")
    }

    /// Path to the GitHub poll state file.
    pub fn github_poll_state_path(&self) -> PathBuf {
        self.state_dir().join("github_poll_state.json")
    }

    /// Path to the Linear poll state file.
    pub fn linear_poll_state_path(&self) -> PathBuf {
        self.state_dir().join("linear_poll_state.json")
    }

    /// Path to the memory distillation state file (spec 21).
    pub fn distillation_state_path(&self) -> PathBuf {
        self.state_dir().join("distillation_state.json")
    }

    /// Path to the per-turn usage ledger.
    pub fn usage_db_path(&self) -> PathBuf {
        self.state_dir().join("usage.db")
    }

    /// Path to the review findings ledger (spec 23).
    pub fn review_db_path(&self) -> PathBuf {
        self.state_dir().join("review.db")
    }

    /// The system prompt, assembled once at workspace init.
    ///
    /// The persona is compiled in; USER.md is provisioned via Nix and
    /// changes require a restart anyway, so caching avoids re-reading it
    /// per turn.
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }
}

/// Assemble the system prompt: the compiled-in persona plus the
/// operator's optional USER.md.
fn read_system_prompt(root: &Path) -> String {
    let mut prompt = PRODUCT_PROMPT.to_string();

    match fs::read_to_string(root.join(USER_PROMPT)) {
        Ok(content) => {
            prompt.push('\n');
            prompt.push_str(&content);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("failed to read {USER_PROMPT}: {e}"),
    }

    prompt
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

/// Append a timestamped entry to an append-only log file.
///
/// Used for the duty history and the notification mirror: durable,
/// human-readable, greppable records under `state/`.
pub fn append_log(path: &std::path::Path, entry: &str) -> std::io::Result<()> {
    use std::io::Write;
    let timestamp = crate::time::now_iso8601();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "[{timestamp}] {entry}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The log used to live at the root under heartbeat ownership. An
    /// existing one moves rather than being abandoned beside a new one.
    #[test]
    fn init_moves_a_legacy_history_into_state() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("HISTORY.md"), "[t] old entry\n").unwrap();

        let ws = Workspace::init_at(dir.path().to_path_buf()).unwrap();

        assert_eq!(ws.history_path(), dir.path().join("state/HISTORY.md"));
        assert_eq!(
            fs::read_to_string(ws.history_path()).unwrap(),
            "[t] old entry\n"
        );
        assert!(!dir.path().join("HISTORY.md").exists());
    }

    /// A log already in place wins; the legacy file is left alone rather
    /// than clobbering newer history.
    #[test]
    fn init_keeps_the_current_history_over_a_legacy_one() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("state")).unwrap();
        fs::write(dir.path().join("HISTORY.md"), "old\n").unwrap();
        fs::write(dir.path().join("state/HISTORY.md"), "current\n").unwrap();

        let ws = Workspace::init_at(dir.path().to_path_buf()).unwrap();

        assert_eq!(fs::read_to_string(ws.history_path()).unwrap(), "current\n");
        assert!(dir.path().join("HISTORY.md").exists());
    }

    #[test]
    fn init_creates_structure() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::init_at(dir.path().to_path_buf()).unwrap();

        assert!(ws.path().join("sessions").is_dir());
        assert!(ws.path().join("memory").is_dir());
        assert!(ws.path().join("memory/topics").is_dir());
        assert!(ws.path().join("projects").is_dir());
        assert!(ws.path().join("state").is_dir());
    }

    #[test]
    fn system_prompt_embeds_persona_and_appends_user() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("USER.md"), "# User Preferences\n").unwrap();
        let ws = Workspace::init_at(dir.path().to_path_buf()).unwrap();

        let prompt = ws.system_prompt();
        assert!(prompt.contains("# Soul"));
        assert!(prompt.contains("# Agent Instructions"));
        assert!(prompt.contains("# User Preferences"));
    }

    #[test]
    fn system_prompt_without_user_is_persona_only() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::init_at(dir.path().to_path_buf()).unwrap();

        let prompt = ws.system_prompt();
        assert!(prompt.contains("# Soul"));
        assert!(prompt.contains("# Agent Instructions"));
        assert!(!prompt.contains("# User Preferences"));
    }
}
