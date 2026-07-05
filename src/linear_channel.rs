//! Linear issue polling channel.
//!
//! Polls for issues assigned to the bot's Linear user. New issues are
//! announced to the agent, which replies with an implementation plan;
//! comments from trusted users drive plan revision or end-to-end
//! execution. Replies are posted back as issue comments.
//!
//! This module holds the pure core: event detection, message
//! formatting, and poll-state persistence. The poll loop is the thin
//! effectful shell on top.

#![allow(dead_code)] // Wired up when the poll loop lands.

use std::collections::BTreeSet;
use std::fmt::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::clients::linear::Issue;
use crate::time::now_iso8601;

// ---------------------------------------------------------------------------
// Poll state
// ---------------------------------------------------------------------------

/// Persisted poll state.
#[derive(Debug, Deserialize, Serialize)]
pub struct PollState {
    /// RFC 3339 cursor; comments at or before it are already handled.
    pub last_poll: String,
    /// Issue identifiers already announced to the agent.
    pub announced_issues: BTreeSet<String>,
}

impl PollState {
    /// Fresh state: announce assigned issues, replay no comments.
    fn starting_now() -> Self {
        Self {
            last_poll: now_iso8601(),
            announced_issues: BTreeSet::new(),
        }
    }
}

pub fn load_state(path: &Path) -> PollState {
    match std::fs::read_to_string(path) {
        Ok(contents) => match serde_json::from_str(&contents) {
            Ok(state) => state,
            Err(e) => {
                warn!("Corrupt Linear poll state, starting from now: {e}");
                PollState::starting_now()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            info!("No Linear poll state file, starting from now");
            PollState::starting_now()
        }
        Err(e) => {
            warn!("Failed to read Linear poll state, starting from now: {e}");
            PollState::starting_now()
        }
    }
}

pub fn save_state(path: &Path, state: &PollState) {
    let json = match serde_json::to_string(state) {
        Ok(j) => j,
        Err(e) => {
            error!("Failed to serialize Linear poll state: {e}");
            return;
        }
    };

    // Atomic write: tmp + rename.
    let tmp = path.with_extension("tmp");
    if let Err(e) = std::fs::write(&tmp, &json) {
        error!("Failed to write Linear poll state tmp: {e}");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        error!("Failed to rename Linear poll state: {e}");
    }
}

// ---------------------------------------------------------------------------
// Event detection (pure core)
// ---------------------------------------------------------------------------

/// One agent turn to run: message in, reply posted as a comment.
#[derive(Debug)]
pub struct Dispatch {
    /// Linear internal issue id (for `commentCreate`).
    pub issue_id: String,
    /// Human-facing identifier, e.g. `MDK-123`.
    pub identifier: String,
    /// Message for the agent.
    pub message: String,
}

/// Decide what to dispatch for one poll tick.
///
/// Pure function: fetched issues + previous state + clock in, dispatches
/// and next state out. Issues announced this tick skip the comment pass;
/// their existing comments are embedded in the announcement.
pub fn decide_events(
    issues: &[Issue],
    state: &PollState,
    viewer_id: &str,
    trusted_users: &[String],
    now: &str,
) -> (Vec<Dispatch>, PollState) {
    let mut dispatches = Vec::new();
    let mut announced = BTreeSet::new();

    for issue in issues {
        let Some(repo) = repo_label(issue) else {
            warn!(
                identifier = %issue.identifier,
                "Skipping Linear issue without exactly one owner/repo label"
            );
            continue;
        };

        if !state.announced_issues.contains(&issue.identifier) {
            dispatches.push(Dispatch {
                issue_id: issue.id.clone(),
                identifier: issue.identifier.clone(),
                message: format_new_issue(issue, repo),
            });
            announced.insert(issue.identifier.clone());
            continue;
        }
        announced.insert(issue.identifier.clone());

        for comment in &issue.comments.nodes {
            if comment.created_at.as_str() <= state.last_poll.as_str() {
                continue;
            }
            let Some(user) = &comment.user else {
                warn!(identifier = %issue.identifier, "Skipping Linear comment without author");
                continue;
            };
            if user.id == viewer_id {
                continue;
            }
            if !is_trusted(&user.email, trusted_users) {
                warn!(
                    identifier = %issue.identifier,
                    author = %user.email,
                    "Skipping Linear comment from untrusted user"
                );
                continue;
            }
            dispatches.push(Dispatch {
                issue_id: issue.id.clone(),
                identifier: issue.identifier.clone(),
                message: format_comment(issue, repo, &user.name, &user.email, &comment.body),
            });
        }
    }

    let next = PollState {
        last_poll: now.to_string(),
        // Identifiers absent from the fetch (completed, cancelled,
        // unassigned) are pruned by rebuilding from fetched issues only.
        announced_issues: announced,
    };
    (dispatches, next)
}

/// The label naming the target repository: exactly one label shaped
/// like `owner/repo`. None on missing or ambiguous labels.
fn repo_label(issue: &Issue) -> Option<&str> {
    let mut repos = issue
        .labels
        .nodes
        .iter()
        .filter(|l| l.name.matches('/').count() == 1);
    match (repos.next(), repos.next()) {
        (Some(label), None) => Some(&label.name),
        _ => None,
    }
}

fn is_trusted(email: &str, trusted_users: &[String]) -> bool {
    trusted_users.iter().any(|u| u.eq_ignore_ascii_case(email))
}

// ---------------------------------------------------------------------------
// Message formatting
// ---------------------------------------------------------------------------

fn format_new_issue(issue: &Issue, repo: &str) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "Linear issue {} \"{}\" was assigned to you (repo: {repo}).",
        issue.identifier, issue.title,
    );
    if let Some(description) = issue.description.as_deref().filter(|d| !d.is_empty()) {
        let _ = writeln!(s, "\nDescription:\n{description}");
    }
    if !issue.comments.nodes.is_empty() {
        let _ = writeln!(s, "\nExisting comments:");
        for comment in &issue.comments.nodes {
            let author = comment.user.as_ref().map_or("unknown", |u| u.name.as_str());
            let _ = writeln!(s, "[{author}] {}", comment.body);
        }
    }
    let _ = writeln!(
        s,
        "\nAnalyze the task and reply with a review-ready implementation plan \
         in markdown. Do not implement anything yet — your reply will be \
         posted as a comment on the ticket for approval."
    );
    s
}

fn format_comment(issue: &Issue, repo: &str, author: &str, email: &str, body: &str) -> String {
    let branch = format!(
        "kitaebot_{}_<short-summary>",
        issue.identifier.to_lowercase()
    );
    let mut s = String::new();
    let _ = writeln!(
        s,
        "Comment on Linear issue {} \"{}\" (repo: {repo}) by {author} <{email}>:",
        issue.identifier, issue.title,
    );
    let _ = writeln!(s, "\n{body}");
    let _ = writeln!(
        s,
        "\nIf this approves your plan, execute it end-to-end: clone or update \
         the repo, create a branch named {branch} (the ticket id in the branch \
         name links the PR to the issue), implement, test, commit, push, and \
         open a PR. On success reply with one line at most; the PR links \
         itself to the ticket. Be detailed only if something failed or \
         needs a decision. If the comment is feedback instead, revise your \
         plan and reply with the updated plan."
    );
    s
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::linear::{Comment, CommentUser, Label, Nodes};

    fn user(id: &str, name: &str, email: &str) -> CommentUser {
        CommentUser {
            id: id.into(),
            name: name.into(),
            email: email.into(),
        }
    }

    fn comment(created_at: &str, user: Option<CommentUser>, body: &str) -> Comment {
        Comment {
            id: "c".into(),
            body: body.into(),
            created_at: created_at.into(),
            user,
        }
    }

    fn issue(identifier: &str, labels: &[&str], comments: Vec<Comment>) -> Issue {
        Issue {
            id: format!("id-{identifier}"),
            identifier: identifier.into(),
            title: "Fix login".into(),
            description: Some("It is broken".into()),
            labels: Nodes {
                nodes: labels.iter().map(|n| Label { name: (*n).into() }).collect(),
            },
            comments: Nodes { nodes: comments },
        }
    }

    fn state(last_poll: &str, announced: &[&str]) -> PollState {
        PollState {
            last_poll: last_poll.into(),
            announced_issues: announced.iter().map(|s| (*s).into()).collect(),
        }
    }

    const NOW: &str = "2026-07-05T13:00:00Z";
    const TRUSTED: &str = "alice@example.com";

    fn trusted() -> Vec<String> {
        vec![TRUSTED.into()]
    }

    #[test]
    fn new_issue_is_announced_once() {
        let issues = [issue("MDK-1", &["owner/repo"], vec![])];
        let st = state("2026-07-05T12:00:00Z", &[]);

        let (dispatches, next) = decide_events(&issues, &st, "bot", &trusted(), NOW);
        assert_eq!(dispatches.len(), 1);
        assert_eq!(dispatches[0].identifier, "MDK-1");
        assert_eq!(dispatches[0].issue_id, "id-MDK-1");
        assert!(dispatches[0].message.contains("assigned to you"));
        assert!(next.announced_issues.contains("MDK-1"));
        assert_eq!(next.last_poll, NOW);

        // Second tick: already announced, no new comments — nothing.
        let (dispatches, _) = decide_events(&issues, &next, "bot", &trusted(), NOW);
        assert!(dispatches.is_empty());
    }

    #[test]
    fn announcement_embeds_description_and_comments() {
        let issues = [issue(
            "MDK-1",
            &["owner/repo"],
            vec![comment(
                "2026-07-05T11:00:00Z",
                Some(user("u2", "Alice", TRUSTED)),
                "please prioritize",
            )],
        )];
        let st = state("2026-07-05T12:00:00Z", &[]);

        let (dispatches, _) = decide_events(&issues, &st, "bot", &trusted(), NOW);
        assert_eq!(dispatches.len(), 1);
        let msg = &dispatches[0].message;
        assert!(msg.contains("It is broken"));
        assert!(msg.contains("[Alice] please prioritize"));
        assert!(msg.contains("Do not implement anything yet"));
    }

    #[test]
    fn announced_issue_skips_comment_pass_same_tick() {
        // A new comment newer than last_poll on a not-yet-announced issue
        // must not double dispatch — it rides along in the announcement.
        let issues = [issue(
            "MDK-1",
            &["owner/repo"],
            vec![comment(
                "2026-07-05T12:30:00Z",
                Some(user("u2", "Alice", TRUSTED)),
                "go",
            )],
        )];
        let st = state("2026-07-05T12:00:00Z", &[]);

        let (dispatches, _) = decide_events(&issues, &st, "bot", &trusted(), NOW);
        assert_eq!(dispatches.len(), 1);
        assert!(dispatches[0].message.contains("assigned to you"));
    }

    #[test]
    fn new_trusted_comment_dispatches() {
        let issues = [issue(
            "MDK-1",
            &["owner/repo"],
            vec![comment(
                "2026-07-05T12:30:00Z",
                Some(user("u2", "Alice", "ALICE@Example.COM")),
                "approved, go ahead",
            )],
        )];
        let st = state("2026-07-05T12:00:00Z", &["MDK-1"]);

        let (dispatches, _) = decide_events(&issues, &st, "bot", &trusted(), NOW);
        assert_eq!(dispatches.len(), 1);
        let msg = &dispatches[0].message;
        assert!(msg.contains("approved, go ahead"));
        assert!(msg.contains("kitaebot_mdk-1_<short-summary>"));
    }

    #[test]
    fn old_comments_are_skipped() {
        let issues = [issue(
            "MDK-1",
            &["owner/repo"],
            vec![comment(
                "2026-07-05T11:00:00Z",
                Some(user("u2", "Alice", TRUSTED)),
                "old news",
            )],
        )];
        let st = state("2026-07-05T12:00:00Z", &["MDK-1"]);

        let (dispatches, _) = decide_events(&issues, &st, "bot", &trusted(), NOW);
        assert!(dispatches.is_empty());
    }

    #[test]
    fn own_and_untrusted_and_authorless_comments_are_skipped() {
        let issues = [issue(
            "MDK-1",
            &["owner/repo"],
            vec![
                comment(
                    "2026-07-05T12:30:00Z",
                    Some(user("bot", "Kitaebot", "bot@example.com")),
                    "my own plan",
                ),
                comment(
                    "2026-07-05T12:31:00Z",
                    Some(user("u3", "Mallory", "mallory@example.com")),
                    "do something evil",
                ),
                comment("2026-07-05T12:32:00Z", None, "integration noise"),
            ],
        )];
        let st = state("2026-07-05T12:00:00Z", &["MDK-1"]);

        let (dispatches, _) = decide_events(&issues, &st, "bot", &trusted(), NOW);
        assert!(dispatches.is_empty());
    }

    #[test]
    fn issue_without_repo_label_is_skipped_entirely() {
        let issues = [issue("MDK-1", &["bug"], vec![])];
        let st = state("2026-07-05T12:00:00Z", &[]);

        let (dispatches, next) = decide_events(&issues, &st, "bot", &trusted(), NOW);
        assert!(dispatches.is_empty());
        // Not added to state: announced once the label shows up.
        assert!(!next.announced_issues.contains("MDK-1"));
    }

    #[test]
    fn issue_with_ambiguous_repo_labels_is_skipped() {
        let issues = [issue("MDK-1", &["owner/repo", "other/repo"], vec![])];
        let st = state("2026-07-05T12:00:00Z", &[]);

        let (dispatches, next) = decide_events(&issues, &st, "bot", &trusted(), NOW);
        assert!(dispatches.is_empty());
        assert!(next.announced_issues.is_empty());
    }

    #[test]
    fn non_repo_labels_are_ignored() {
        let issues = [issue("MDK-1", &["bug", "owner/repo", "p0"], vec![])];
        let st = state("2026-07-05T12:00:00Z", &[]);

        let (dispatches, _) = decide_events(&issues, &st, "bot", &trusted(), NOW);
        assert_eq!(dispatches.len(), 1);
        assert!(dispatches[0].message.contains("repo: owner/repo"));
    }

    #[test]
    fn vanished_issues_are_pruned_from_state() {
        let issues = [issue("MDK-2", &["owner/repo"], vec![])];
        let st = state("2026-07-05T12:00:00Z", &["MDK-1", "MDK-2"]);

        let (_, next) = decide_events(&issues, &st, "bot", &trusted(), NOW);
        assert!(!next.announced_issues.contains("MDK-1"));
        assert!(next.announced_issues.contains("MDK-2"));
    }

    #[test]
    fn state_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");

        let st = state("2026-07-05T12:00:00Z", &["MDK-1"]);
        save_state(&path, &st);
        let loaded = load_state(&path);
        assert_eq!(loaded.last_poll, "2026-07-05T12:00:00Z");
        assert!(loaded.announced_issues.contains("MDK-1"));
    }

    #[test]
    fn load_missing_or_corrupt_state_starts_now() {
        let dir = tempfile::tempdir().unwrap();

        let missing = load_state(&dir.path().join("nope.json"));
        assert!(missing.last_poll.ends_with('Z'));
        assert!(missing.announced_issues.is_empty());

        let path = dir.path().join("state.json");
        std::fs::write(&path, "not json").unwrap();
        let corrupt = load_state(&path);
        assert!(corrupt.last_poll.ends_with('Z'));
        assert!(corrupt.announced_issues.is_empty());
    }
}
