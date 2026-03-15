//! GitHub PR polling channel.
//!
//! Polls for the bot's own open PRs across all repos. For each PR,
//! fetches reviews, comments, and inline diff comments newer than
//! `last_poll`. Sends each new item through the [`AgentHandle`].
//! Skips the bot's own messages to avoid infinite loops.

use std::fmt::Write;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use tokio::time::{self, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::agent::AgentHandle;
use crate::agent::envelope::ChannelSource;
use crate::error::ToolError;
use crate::time::now_iso8601;
use crate::tools::github::GhCli;

// ---------------------------------------------------------------------------
// Types — channel-specific, intentionally duplicating tool types.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct GhUser {
    login: String,
}

#[derive(Deserialize)]
struct SearchResult {
    number: u32,
    title: String,
    repository: Repository,
}

#[derive(Deserialize)]
struct Repository {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Review {
    author: Author,
    body: String,
    state: String,
    submitted_at: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrComment {
    author: Author,
    body: String,
    created_at: String,
}

#[derive(Deserialize)]
struct DiffComment {
    path: String,
    line: Option<u64>,
    body: String,
    user: Author,
    created_at: String,
}

#[derive(Deserialize)]
struct Author {
    login: String,
}

/// Aggregate response from `gh pr view --json reviews,comments`.
#[derive(Deserialize)]
struct PrViewResponse {
    reviews: Vec<Review>,
    comments: Vec<PrComment>,
}

/// Persisted poll state.
#[derive(Deserialize, serde::Serialize)]
struct PollState {
    last_poll: String,
}

// ---------------------------------------------------------------------------
// Poll loop
// ---------------------------------------------------------------------------

/// Run the GitHub PR polling loop forever.
///
/// On first boot (or missing state file), `last_poll` is set to "now"
/// so we don't replay entire PR histories.
pub async fn poll_loop(
    gh: &GhCli,
    interval: Duration,
    handle: &AgentHandle,
    state_path: &Path,
) -> ! {
    let bot_login = match resolve_bot_login(gh).await {
        Ok(login) => {
            info!(login = %login, "GitHub channel resolved bot identity");
            login
        }
        Err(e) => {
            error!("GitHub channel: failed to resolve bot login: {e}");
            std::future::pending().await
        }
    };

    let mut last_poll = load_last_poll(state_path);
    info!(last_poll = %last_poll, "GitHub channel starting");

    let mut tick = time::interval(interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tick.tick().await;
        match poll_once(gh, handle, &bot_login, &last_poll).await {
            Ok(count) => {
                info!(count, "GitHub poll: dispatched {count} items");
                last_poll = now_iso8601();
                save_last_poll(state_path, &last_poll);
            }
            Err(e) => {
                error!("GitHub poll error (will retry next tick): {e}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Core polling logic
// ---------------------------------------------------------------------------

async fn poll_once(
    gh: &GhCli,
    handle: &AgentHandle,
    bot_login: &str,
    last_poll: &str,
) -> Result<usize, ToolError> {
    let prs = list_bot_prs(gh).await?;
    let mut count = 0;

    for pr in &prs {
        let nwo = &pr.repository.name_with_owner;

        let view = fetch_pr_view(gh, nwo, pr.number).await?;
        let diff_comments = fetch_diff_comments(gh, nwo, pr.number).await?;

        for review in &view.reviews {
            if review.author.login == bot_login {
                continue;
            }
            if review.submitted_at.as_str() <= last_poll {
                continue;
            }
            send(handle, pr.number, format_review(pr, nwo, review)).await;
            count += 1;
        }

        for comment in &view.comments {
            if comment.author.login == bot_login {
                continue;
            }
            if comment.created_at.as_str() <= last_poll {
                continue;
            }
            send(handle, pr.number, format_comment(pr, nwo, comment)).await;
            count += 1;
        }

        for dc in &diff_comments {
            if dc.user.login == bot_login {
                continue;
            }
            if dc.created_at.as_str() <= last_poll {
                continue;
            }
            send(handle, pr.number, format_diff_comment(pr, nwo, dc)).await;
            count += 1;
        }
    }

    Ok(count)
}

async fn send(handle: &AgentHandle, pr_number: u32, message: String) {
    let cancel = CancellationToken::new();
    match handle
        .send_message(ChannelSource::GitHub { pr_number }, message, None, cancel)
        .await
    {
        Ok(reply) => info!(pr_number, "GitHub PR #{pr_number}: {}", reply.content),
        Err(e) => error!(pr_number, "GitHub PR #{pr_number} error: {e}"),
    }
}

// ---------------------------------------------------------------------------
// gh CLI calls
// ---------------------------------------------------------------------------

async fn resolve_bot_login(gh: &GhCli) -> Result<String, ToolError> {
    let call = gh.prepare_gh(&["api", "user"], gh.workspace_root());
    let user: GhUser = gh.exec_parse(&call).await?;
    Ok(user.login)
}

async fn list_bot_prs(gh: &GhCli) -> Result<Vec<SearchResult>, ToolError> {
    let call = gh.prepare_gh(
        &[
            "search",
            "prs",
            "--author=@me",
            "--state=open",
            "--json",
            "number,title,repository",
        ],
        gh.workspace_root(),
    );
    gh.exec_parse(&call).await
}

async fn fetch_pr_view(gh: &GhCli, nwo: &str, pr_number: u32) -> Result<PrViewResponse, ToolError> {
    let number = pr_number.to_string();
    let repo_flag = format!("-R{nwo}");
    let call = gh.prepare_gh(
        &[
            "pr",
            "view",
            &number,
            &repo_flag,
            "--json",
            "reviews,comments",
        ],
        gh.workspace_root(),
    );
    gh.exec_parse(&call).await
}

async fn fetch_diff_comments(
    gh: &GhCli,
    nwo: &str,
    pr_number: u32,
) -> Result<Vec<DiffComment>, ToolError> {
    let endpoint = format!("repos/{nwo}/pulls/{pr_number}/comments");
    let call = gh.prepare_gh(&["api", &endpoint], gh.workspace_root());
    gh.exec_parse(&call).await
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

fn format_review(pr: &SearchResult, nwo: &str, review: &Review) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "Review on PR #{} \"{}\" ({nwo}) by @{}: {}",
        pr.number, pr.title, review.author.login, review.state,
    );
    if !review.body.is_empty() {
        let _ = writeln!(s, "\n{}", review.body);
    }
    s
}

fn format_comment(pr: &SearchResult, nwo: &str, comment: &PrComment) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "Comment on PR #{} \"{}\" ({nwo}) by @{}:",
        pr.number, pr.title, comment.author.login,
    );
    let _ = writeln!(s, "\n{}", comment.body);
    s
}

fn format_diff_comment(pr: &SearchResult, nwo: &str, dc: &DiffComment) -> String {
    let location = dc
        .line
        .map_or(dc.path.clone(), |l| format!("{}:{l}", dc.path));
    let mut s = String::new();
    let _ = writeln!(
        s,
        "Inline comment on PR #{} \"{}\" ({nwo}) by @{} at {location}:",
        pr.number, pr.title, dc.user.login,
    );
    let _ = writeln!(s, "\n{}", dc.body);
    s
}

// ---------------------------------------------------------------------------
// State persistence
// ---------------------------------------------------------------------------

fn load_last_poll(path: &Path) -> String {
    match std::fs::read_to_string(path) {
        Ok(contents) => match serde_json::from_str::<PollState>(&contents) {
            Ok(state) => state.last_poll,
            Err(e) => {
                warn!("Corrupt poll state, starting from now: {e}");
                now_iso8601()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            info!("No poll state file, starting from now");
            now_iso8601()
        }
        Err(e) => {
            warn!("Failed to read poll state, starting from now: {e}");
            now_iso8601()
        }
    }
}

fn save_last_poll(path: &Path, timestamp: &str) {
    let state = PollState {
        last_poll: timestamp.to_string(),
    };
    let json = match serde_json::to_string(&state) {
        Ok(j) => j,
        Err(e) => {
            error!("Failed to serialize poll state: {e}");
            return;
        }
    };

    // Atomic write: tmp + rename.
    let tmp = path.with_extension("tmp");
    if let Err(e) = std::fs::write(&tmp, &json) {
        error!("Failed to write poll state tmp: {e}");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        error!("Failed to rename poll state: {e}");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_review_approved() {
        let pr = SearchResult {
            number: 5,
            title: "Add feature".to_string(),
            repository: Repository {
                name_with_owner: "owner/repo".to_string(),
            },
        };
        let review = Review {
            author: Author {
                login: "alice".to_string(),
            },
            body: "Looks good!".to_string(),
            state: "APPROVED".to_string(),
            submitted_at: "2025-01-15T10:00:00Z".to_string(),
        };
        let result = format_review(&pr, "owner/repo", &review);
        assert_eq!(
            result,
            "Review on PR #5 \"Add feature\" (owner/repo) by @alice: APPROVED\n\nLooks good!\n"
        );
    }

    #[test]
    fn format_review_empty_body() {
        let pr = SearchResult {
            number: 3,
            title: "Fix bug".to_string(),
            repository: Repository {
                name_with_owner: "o/r".to_string(),
            },
        };
        let review = Review {
            author: Author {
                login: "bob".to_string(),
            },
            body: String::new(),
            state: "CHANGES_REQUESTED".to_string(),
            submitted_at: "2025-01-15T10:00:00Z".to_string(),
        };
        let result = format_review(&pr, "o/r", &review);
        assert_eq!(
            result,
            "Review on PR #3 \"Fix bug\" (o/r) by @bob: CHANGES_REQUESTED\n"
        );
    }

    #[test]
    fn format_comment_basic() {
        let pr = SearchResult {
            number: 7,
            title: "Update docs".to_string(),
            repository: Repository {
                name_with_owner: "owner/repo".to_string(),
            },
        };
        let comment = PrComment {
            author: Author {
                login: "carol".to_string(),
            },
            body: "What about edge cases?".to_string(),
            created_at: "2025-01-15T11:00:00Z".to_string(),
        };
        let result = format_comment(&pr, "owner/repo", &comment);
        assert_eq!(
            result,
            "Comment on PR #7 \"Update docs\" (owner/repo) by @carol:\n\nWhat about edge cases?\n"
        );
    }

    #[test]
    fn format_diff_comment_with_line() {
        let pr = SearchResult {
            number: 2,
            title: "Refactor".to_string(),
            repository: Repository {
                name_with_owner: "o/r".to_string(),
            },
        };
        let dc = DiffComment {
            path: "src/main.rs".to_string(),
            line: Some(42),
            body: "Nit: rename this".to_string(),
            user: Author {
                login: "dave".to_string(),
            },
            created_at: "2025-01-15T12:00:00Z".to_string(),
        };
        let result = format_diff_comment(&pr, "o/r", &dc);
        assert_eq!(
            result,
            "Inline comment on PR #2 \"Refactor\" (o/r) by @dave at src/main.rs:42:\n\nNit: rename this\n"
        );
    }

    #[test]
    fn format_diff_comment_no_line() {
        let pr = SearchResult {
            number: 2,
            title: "Refactor".to_string(),
            repository: Repository {
                name_with_owner: "o/r".to_string(),
            },
        };
        let dc = DiffComment {
            path: "src/lib.rs".to_string(),
            line: None,
            body: "Outdated".to_string(),
            user: Author {
                login: "eve".to_string(),
            },
            created_at: "2025-01-15T12:00:00Z".to_string(),
        };
        let result = format_diff_comment(&pr, "o/r", &dc);
        assert_eq!(
            result,
            "Inline comment on PR #2 \"Refactor\" (o/r) by @eve at src/lib.rs:\n\nOutdated\n"
        );
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");

        save_last_poll(&path, "2025-01-15T10:00:00Z");
        let loaded = load_last_poll(&path);
        assert_eq!(loaded, "2025-01-15T10:00:00Z");
    }

    #[test]
    fn load_missing_file_returns_now() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");

        let loaded = load_last_poll(&path);
        // Should be a valid ISO 8601 timestamp (not empty, not an error).
        assert!(loaded.ends_with('Z'));
        assert!(loaded.contains('T'));
    }

    #[test]
    fn load_corrupt_file_returns_now() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "not json at all").unwrap();

        let loaded = load_last_poll(&path);
        assert!(loaded.ends_with('Z'));
        assert!(loaded.contains('T'));
    }
}
