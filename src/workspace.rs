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

/// Workspace-root entries the daemon owns: the model's tools read
/// them but never write them. `PathGuard` and the exec deny list build
/// their fence from these names, so a rename here moves the fence
/// with the directory instead of silently detaching it.
pub const CONFIG_FILE: &str = "config.toml";
pub const CONTEXT_DIR: &str = "context";
/// Repository checkouts live under this directory.
pub const PROJECTS_DIR: &str = "projects";
/// Review worktrees prepared by the GitHub channel live here.
pub const REVIEWS_DIR: &str = "reviews";
pub const STATE_DIR: &str = "state";
/// Ephemeral `GIT_ASKPASS` helpers, relative to [`STATE_DIR`]. Under
/// `state/` so the exec tier kernel-denies it; the git tier grants it
/// read + execute (spec 15).
pub const ASKPASS_DIR: &str = "askpass";
/// The error tee's rolled files, relative to [`STATE_DIR`].
pub const ERRORS_SUBDIR: &str = "errors";
/// The one model-writable file under [`STATE_DIR`], maintained by the
/// review gates. Relative to [`STATE_DIR`].
pub const REVIEW_CHECKLIST: &str = "review-checklist.md";

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
        let path = resolve_root().map_err(|e| WorkspaceError::Init(PathBuf::from(APP_NAME), e))?;
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
        mk(&path.join(CONTEXT_DIR))?;
        mk(&path.join("memory"))?;
        mk(&path.join("memory/topics"))?;
        mk(&path.join(PROJECTS_DIR))?;
        // The git tier's Landlock rule can only grant an existing path.
        mk(&path.join(REVIEWS_DIR))?;
        mk(&path.join(STATE_DIR))?;
        seed_review_checklist(&path)?;

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

    /// Directory owned by the active context engine (spec 14): its
    /// store, sessions, and cursors, laid out however the engine
    /// chooses. The workspace hands over the path and looks no deeper.
    pub fn context_dir(&self) -> PathBuf {
        self.root.join(CONTEXT_DIR)
    }

    /// Directory holding machine-owned runtime state (engine store,
    /// channel poll cursors).
    pub fn state_dir(&self) -> PathBuf {
        self.root.join(STATE_DIR)
    }

    /// Directory holding the memory subsystem's files (spec 21).
    pub fn memory_dir(&self) -> PathBuf {
        self.root.join("memory")
    }

    /// Path to the journal: the append-only, topic-tagged record of
    /// what the bot did — duty outcomes, unattended replies, sent
    /// notifications. Greppable by topic (`[duty]`, `[notify]`, ...).
    pub fn journal_path(&self) -> PathBuf {
        self.state_dir().join("JOURNAL.md")
    }

    /// Directory of the error tee's rolled JSON files (WARN/ERROR
    /// events), the self-analysis duty's symptom source. Written by
    /// tracing from process start via [`resolve_root`].
    pub fn errors_dir(&self) -> PathBuf {
        self.state_dir().join(ERRORS_SUBDIR)
    }

    /// Path to the operational state database (usage ledger, review
    /// ledger, doc store).
    pub fn state_db_path(&self) -> PathBuf {
        self.state_dir().join("kitaebot.db")
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

/// Header seeded into an absent review checklist.
///
/// The reviewer reads the checklist at every gate and the review gates
/// append to it, so it exists from first boot rather than from the
/// first escape. Present-and-empty says "no lessons yet"; absent said
/// the same thing through a failed read.
const REVIEW_CHECKLIST_HEADER: &str = "\
# Review checklist

One line per recurring failure class, stating what to check rather than
what happened. Maintained by the review gates from escapes; read by the
reviewer before judging.
";

/// Create the review checklist if it is missing, preserving whatever a
/// previous run wrote.
fn seed_review_checklist(root: &Path) -> Result<(), WorkspaceError> {
    let path = root.join(STATE_DIR).join(REVIEW_CHECKLIST);
    if path.exists() {
        return Ok(());
    }
    fs::write(&path, REVIEW_CHECKLIST_HEADER).map_err(|e| WorkspaceError::Init(path, e))
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

/// Workspace root from `KITAEBOT_WORKSPACE` or the XDG default,
/// resolved without touching the filesystem. The error tee needs the
/// path before the workspace initializes, so tracing can capture
/// workspace-init failures themselves.
pub fn resolve_root() -> Result<PathBuf, std::io::Error> {
    std::env::var(ENV_VAR)
        .map(PathBuf::from)
        .or_else(|_| default_data_dir())
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

/// Cap on one journal entry. Matches the notifier's send cap so a
/// mirrored notification is never shorter in the journal than on the
/// phone; long unattended replies are excerpted, not reproduced.
pub const JOURNAL_ENTRY_MAX: usize = 4000;

/// Append a timestamped, topic-tagged entry to the journal.
///
/// `[timestamp] [topic] entry`, one entry per event, capped at
/// [`JOURNAL_ENTRY_MAX`]. Topics keep one file greppable per concern;
/// the admission rule is spec 05's: work performed, outcomes,
/// failures, and messages sent to a human — never routine no-ops.
pub fn journal(path: &std::path::Path, topic: &str, entry: &str) -> std::io::Result<()> {
    use std::io::Write;
    let timestamp = crate::time::now_iso8601();
    let entry = crate::tools::truncate_output(entry, JOURNAL_ENTRY_MAX);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "[{timestamp}] [{topic}] {entry}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_entries_are_topic_tagged_and_capped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("JOURNAL.md");

        journal(&path, "duty", "warm: all repos warm").unwrap();
        journal(&path, "notify", &"x".repeat(JOURNAL_ENTRY_MAX + 500)).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("] [duty] warm: all repos warm"));
        assert!(text.contains("] [notify] x"));
        assert!(text.contains("[truncated"), "oversized entries are capped");
    }

    #[test]
    fn init_creates_structure() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::init_at(dir.path().to_path_buf()).unwrap();

        assert!(ws.path().join("context").is_dir());
        assert!(ws.path().join("memory").is_dir());
        assert!(ws.path().join("memory/topics").is_dir());
        assert!(ws.path().join(PROJECTS_DIR).is_dir());
        assert!(ws.path().join("state").is_dir());
        assert!(ws.path().join("state").join(REVIEW_CHECKLIST).is_file());
    }

    /// The gates append lessons to this file; re-initializing on every
    /// daemon start must not wipe them.
    #[test]
    fn init_keeps_an_existing_review_checklist() {
        let dir = tempfile::tempdir().unwrap();
        Workspace::init_at(dir.path().to_path_buf()).unwrap();
        let path = dir.path().join("state").join(REVIEW_CHECKLIST);
        fs::write(&path, "- check that errors name the operation\n").unwrap();

        Workspace::init_at(dir.path().to_path_buf()).unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "- check that errors name the operation\n"
        );
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
