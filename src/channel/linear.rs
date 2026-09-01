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

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::{self, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::execution_checkout;
use crate::agent::AgentHandle;
use crate::agent::envelope::{ChannelSource, TurnRole};
use crate::clients::linear::{Issue, LinearClient};
use crate::error::LinearError;
use crate::state_db::StateDb;
use crate::time::now_iso8601;
use crate::tools::git::GitCli;

// ---------------------------------------------------------------------------
// Channel
// ---------------------------------------------------------------------------

/// Maximum retries for `commentCreate` on transient failures.
const POST_RETRIES: u32 = 3;

/// Linear issue polling channel.
pub struct LinearChannel {
    client: LinearClient,
    interval: Duration,
    trusted_users: Vec<String>,
    /// Label requesting plan-first choreography (config
    /// `linear.plan_label`).
    plan_label: String,
    /// Prepares a fresh base checkout for execution turns. `None` when
    /// the GitHub token is unavailable (agent clones for itself).
    git: Option<GitCli>,
}

impl LinearChannel {
    pub fn new(
        client: LinearClient,
        interval: Duration,
        trusted_users: Vec<String>,
        plan_label: String,
        git: Option<GitCli>,
    ) -> Self {
        Self {
            client,
            interval,
            trusted_users,
            plan_label,
            git,
        }
    }

    /// Post a comment with retries on transient failures; returns the
    /// created comment's id.
    ///
    /// Retries up to [`POST_RETRIES`] times with exponential backoff
    /// (1s, 2s, 4s) on network errors; 429/5xx surface as
    /// [`LinearError::Network`] from the client.
    async fn post_comment(&self, issue_id: &str, body: &str) -> Result<String, LinearError> {
        let mut attempts = 0u32;
        loop {
            match self.client.create_comment(issue_id, body).await {
                Ok(id) => return Ok(id),
                Err(e) if attempts < POST_RETRIES && is_transient(&e) => {
                    let delay = Duration::from_secs(u64::from(1u32 << attempts));
                    attempts += 1;
                    warn!(
                        attempt = attempts,
                        "create_comment retrying in {delay:?}: {e}"
                    );
                    time::sleep(delay).await;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// Whether a [`LinearError`] is worth retrying.
fn is_transient(err: &LinearError) -> bool {
    matches!(err, LinearError::Network(_))
}

// ---------------------------------------------------------------------------
// Poll loop
// ---------------------------------------------------------------------------

/// Run the Linear polling loop forever.
///
/// Resolves the viewer once at startup; failure disables the channel
/// (logged, then pending forever) rather than crashing the daemon.
pub async fn poll_loop(channel: &LinearChannel, handle: &AgentHandle, state_db: &StateDb) -> ! {
    let viewer = match channel.client.viewer().await {
        Ok(v) => {
            info!(name = %v.name, email = %v.email, "Linear channel resolved bot identity");
            v
        }
        Err(e) => {
            error!("Linear channel: failed to resolve viewer: {e}");
            std::future::pending().await
        }
    };

    let mut state = load_state(state_db);
    info!(last_poll = %state.last_poll, "Linear channel starting");

    let mut tick = time::interval(channel.interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tick.tick().await;
        let issues = match channel.client.assigned_issues().await {
            Ok(issues) => issues,
            Err(e) => {
                error!("Linear poll error (will retry next tick): {e}");
                continue;
            }
        };

        let (dispatches, next) = decide_events(
            &issues,
            &state,
            &viewer.id,
            &channel.trusted_users,
            &channel.plan_label,
            &now_iso8601(),
        );
        let count = dispatches.len();
        let mut next = next;
        for d in dispatches {
            let key = d.identifier.clone();
            if let Some(plan_id) = dispatch(channel, handle, d).await {
                next.plan_comments.insert(key, plan_id);
            }
        }
        info!(count, "Linear poll: dispatched {count} items");

        state = next;
        save_state(state_db, &state);
    }
}

/// Prepare a fresh base checkout for an execution turn and describe it
/// for the agent, or `None` when the turn needs no checkout.
async fn checkout_note(channel: &LinearChannel, d: &Dispatch) -> Option<String> {
    if d.kind != TurnKind::Execution {
        return None;
    }
    let Some(git) = &channel.git else {
        return Some(execution_checkout::CLONE_YOURSELF.into());
    };
    match execution_checkout::prepare(git, &d.repo).await {
        Ok(prepared) => Some(prepared.ready_note()),
        Err(e) => {
            warn!(identifier = %d.identifier, "execution checkout prep failed: {e}");
            Some(execution_checkout::CLONE_YOURSELF.into())
        }
    }
}

/// Run one agent turn and post the reply (or error) as a comment.
/// Returns the posted comment's id for plan announcements — that
/// comment is the plan, and post-plan dispatches embed it.
async fn dispatch(channel: &LinearChannel, handle: &AgentHandle, d: Dispatch) -> Option<String> {
    let cancel = CancellationToken::new();
    let source = ChannelSource::Linear {
        issue: d.identifier.clone(),
    };
    let message = match checkout_note(channel, &d).await {
        Some(note) => format!("{}\n\n{note}", d.message),
        None => d.message.clone(),
    };
    // Route per-repo (the issue's owner/repo label): the actor switches
    // to that session for the turn, so all of a repo's tickets — and its
    // GitHub PRs, which use the same key — share one session.
    let body = match handle
        .send_message_with_role(
            source,
            message,
            Some(d.repo.clone()),
            None,
            cancel,
            match d.kind {
                TurnKind::Execution => TurnRole::Default,
                TurnKind::Plan => TurnRole::Planner,
            },
        )
        .await
    {
        Ok(reply) => {
            info!("Linear {}: {}", d.identifier, reply.content);
            reply.content
        }
        Err(e) => {
            error!("Linear {} error: {e}", d.identifier);
            e
        }
    };
    match channel.post_comment(&d.issue_id, &body).await {
        Ok(posted) => (d.kind == TurnKind::Plan).then_some(posted),
        Err(e) => {
            error!("Linear {}: failed to post comment: {e}", d.identifier);
            None
        }
    }
}

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
    /// The bot's plan comment per issue, keyed by identifier — the
    /// announcement reply's comment id, embedded in post-plan
    /// dispatches so execution never depends on session context.
    #[serde(default)]
    pub plan_comments: BTreeMap<String, String>,
}

impl PollState {
    /// Fresh state: announce assigned issues, replay no comments.
    fn starting_now() -> Self {
        Self {
            last_poll: now_iso8601(),
            announced_issues: BTreeSet::new(),
            plan_comments: BTreeMap::new(),
        }
    }
}

const DOC: &str = "linear_poll";

pub fn load_state(db: &StateDb) -> PollState {
    db.load_json(DOC, || {
        info!("No Linear poll state, starting from now");
        PollState::starting_now()
    })
}

pub fn save_state(db: &StateDb, state: &PollState) {
    db.save_json(DOC, state);
}

// ---------------------------------------------------------------------------
// Event detection (pure core)
// ---------------------------------------------------------------------------

/// What a dispatched turn is for; each kind gets different plumbing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnKind {
    /// May implement: a fresh base checkout is prepared first.
    Execution,
    /// Plan-first announcement: no checkout, thinks on the planner
    /// override (spec 25's plan/execute split applies to both ticket
    /// channels alike).
    Plan,
}

/// One agent turn to run: message in, reply posted as a comment.
#[derive(Debug)]
pub struct Dispatch {
    /// Linear internal issue id (for `commentCreate`).
    pub issue_id: String,
    /// Human-facing identifier, e.g. `MDK-123`.
    pub identifier: String,
    /// The issue's `owner/repo` label — the session routing key,
    /// shared with the GitHub channel so a repo's PRs and tickets
    /// land in the same session.
    pub repo: String,
    /// Message for the agent.
    pub message: String,
    pub kind: TurnKind,
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
    plan_label: &str,
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
            // The label chooses the choreography: plan-first when the
            // human asked for one, direct execution otherwise.
            let plan_first = has_label(issue, plan_label);
            dispatches.push(Dispatch {
                issue_id: issue.id.clone(),
                identifier: issue.identifier.clone(),
                repo: repo.to_string(),
                message: if plan_first {
                    format_new_issue(issue, repo)
                } else {
                    format_new_issue_execute(issue, repo)
                },
                kind: if plan_first {
                    TurnKind::Plan
                } else {
                    TurnKind::Execution
                },
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
                repo: repo.to_string(),
                message: format_comment(
                    issue,
                    repo,
                    &user.name,
                    &user.email,
                    &comment.body,
                    state
                        .plan_comments
                        .get(&issue.identifier)
                        .map(String::as_str),
                ),
                kind: TurnKind::Execution,
            });
        }
    }

    let next = PollState {
        last_poll: now.to_string(),
        // Identifiers absent from the fetch (completed, cancelled,
        // unassigned) are pruned by rebuilding from fetched issues
        // only; plan ids follow the same lifetime.
        plan_comments: state
            .plan_comments
            .iter()
            .filter(|(k, _)| announced.contains(*k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
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

/// Whether the issue carries `label`, matched case-insensitively.
fn has_label(issue: &Issue, label: &str) -> bool {
    issue
        .labels
        .nodes
        .iter()
        .any(|l| l.name.eq_ignore_ascii_case(label))
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
        "\n{} If this workflow has a plan-review state, move the ticket \
         there with the linear_set_state tool (it lists the available \
         states); otherwise leave the state as-is.",
        super::PLAN_INSTRUCTIONS
    );
    s
}

/// The direct-execution announcement, for issues assigned without
/// the plan label.
fn format_new_issue_execute(issue: &Issue, repo: &str) -> String {
    let branch = format!(
        "kitaebot_{}_<short-summary>",
        issue.identifier.to_lowercase()
    );
    let mut s = String::new();
    let _ = writeln!(
        s,
        "Linear issue {} \"{}\" was assigned to you for direct execution \
         (no plan requested; repo: {repo}).",
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
        "\nImplement it end-to-end: move the ticket to an in-progress \
         state with the linear_set_state tool if the workflow has one, \
         then create a branch named {branch} (the ticket id in the branch \
         name links the PR to the issue), implement, test, commit, push, \
         and open a PR. On success reply with one line at most; the PR \
         links itself to the ticket. Be detailed only if something failed \
         or needs a decision. If the ticket turns out underspecified or \
         materially larger than it reads, stop before implementing and \
         reply with your plan or questions instead — your reply is posted \
         verbatim as a comment on the ticket."
    );
    s
}

fn format_comment(
    issue: &Issue,
    repo: &str,
    author: &str,
    email: &str,
    body: &str,
    plan_comment: Option<&str>,
) -> String {
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
    // The plan rides the dispatch so the execution turn never depends
    // on it surviving session compaction; the comments were fetched
    // this tick, so the lookup is free. A plan past the 100-comment
    // fetch cap (or deleted) degrades to the id reference.
    match plan_comment.map(|id| (id, issue.comments.nodes.iter().find(|c| c.id == id))) {
        Some((id, Some(plan))) => {
            let _ = writeln!(s, "\nYour current plan (comment {id}):\n\n{}", plan.body);
        }
        Some((id, None)) => {
            let _ = writeln!(
                s,
                "\nYou posted a plan earlier (comment {id}), but it is not \
                 among the comments fetched this tick."
            );
        }
        None => {}
    }
    let _ = writeln!(
        s,
        "\nIf this approves your plan, execute it end-to-end: move the ticket \
         to an in-progress state with the linear_set_state tool if the \
         workflow has one, then create a branch named {branch} (the ticket \
         id in the branch name links the PR to the issue), implement, test, \
         commit, push, and open a PR. On success reply with one line at \
         most; the PR links itself to the ticket. Be detailed only if \
         something failed or needs a decision. If the comment is feedback \
         instead, revise your plan and reply with the updated plan."
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
            id: format!("c-{created_at}"),
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
            plan_comments: BTreeMap::new(),
        }
    }

    const NOW: &str = "2026-07-05T13:00:00Z";
    const TRUSTED: &str = "alice@example.com";

    fn trusted() -> Vec<String> {
        vec![TRUSTED.into()]
    }

    #[test]
    fn new_issue_is_announced_once() {
        let issues = [issue("MDK-1", &["owner/repo", "needs-plan"], vec![])];
        let st = state("2026-07-05T12:00:00Z", &[]);

        let (dispatches, next) = decide_events(&issues, &st, "bot", &trusted(), "needs-plan", NOW);
        assert_eq!(dispatches.len(), 1);
        assert_eq!(dispatches[0].identifier, "MDK-1");
        assert_eq!(dispatches[0].issue_id, "id-MDK-1");
        assert_eq!(dispatches[0].repo, "owner/repo");
        assert!(dispatches[0].message.contains("assigned to you"));
        assert_eq!(dispatches[0].kind, TurnKind::Plan);
        assert!(next.announced_issues.contains("MDK-1"));
        assert_eq!(next.last_poll, NOW);

        // Second tick: already announced, no new comments — nothing.
        let (dispatches, _) = decide_events(&issues, &next, "bot", &trusted(), "needs-plan", NOW);
        assert!(dispatches.is_empty());
    }

    #[test]
    fn unlabeled_issue_executes_directly() {
        let issues = [issue("MDK-1", &["owner/repo"], vec![])];
        let st = state("2026-07-05T12:00:00Z", &[]);

        let (dispatches, next) = decide_events(&issues, &st, "bot", &trusted(), "needs-plan", NOW);

        assert_eq!(dispatches.len(), 1);
        assert_eq!(
            dispatches[0].kind,
            TurnKind::Execution,
            "direct execution needs a checkout"
        );
        let msg = &dispatches[0].message;
        assert!(msg.contains("direct execution"), "{msg}");
        assert!(msg.contains("kitaebot_mdk-1_<short-summary>"));
        assert!(
            msg.contains("stop before implementing"),
            "needs the escape hatch"
        );
        assert!(!msg.contains("Do not implement anything yet"));
        assert!(next.announced_issues.contains("MDK-1"));
    }

    #[test]
    fn plan_label_is_case_insensitive() {
        let issues = [issue("MDK-1", &["owner/repo", "Needs-Plan"], vec![])];
        let st = state("2026-07-05T12:00:00Z", &[]);

        let (dispatches, _) = decide_events(&issues, &st, "bot", &trusted(), "needs-plan", NOW);

        assert_eq!(dispatches[0].kind, TurnKind::Plan);
        assert!(
            dispatches[0]
                .message
                .contains("Do not implement anything yet")
        );
    }

    #[test]
    fn announcement_embeds_description_and_comments() {
        let issues = [issue(
            "MDK-1",
            &["owner/repo", "needs-plan"],
            vec![comment(
                "2026-07-05T11:00:00Z",
                Some(user("u2", "Alice", TRUSTED)),
                "please prioritize",
            )],
        )];
        let st = state("2026-07-05T12:00:00Z", &[]);

        let (dispatches, _) = decide_events(&issues, &st, "bot", &trusted(), "needs-plan", NOW);
        assert_eq!(dispatches.len(), 1);
        let msg = &dispatches[0].message;
        assert!(msg.contains("It is broken"));
        assert!(msg.contains("[Alice] please prioritize"));
        assert!(msg.contains("Do not implement anything yet"));
        assert!(msg.contains("posted verbatim"));
        assert!(msg.contains("linear_set_state"));
    }

    #[test]
    fn announced_issue_skips_comment_pass_same_tick() {
        // A new comment newer than last_poll on a not-yet-announced issue
        // must not double dispatch — it rides along in the announcement.
        let issues = [issue(
            "MDK-1",
            &["owner/repo", "needs-plan"],
            vec![comment(
                "2026-07-05T12:30:00Z",
                Some(user("u2", "Alice", TRUSTED)),
                "go",
            )],
        )];
        let st = state("2026-07-05T12:00:00Z", &[]);

        let (dispatches, _) = decide_events(&issues, &st, "bot", &trusted(), "needs-plan", NOW);
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

        let (dispatches, _) = decide_events(&issues, &st, "bot", &trusted(), "needs-plan", NOW);
        assert_eq!(dispatches.len(), 1);
        assert_eq!(dispatches[0].repo, "owner/repo");
        assert_eq!(dispatches[0].kind, TurnKind::Execution);
        let msg = &dispatches[0].message;
        assert!(msg.contains("approved, go ahead"));
        assert!(msg.contains("kitaebot_mdk-1_<short-summary>"));
        assert!(msg.contains("in-progress state with the linear_set_state tool"));
    }

    /// The dispatch embeds the plan body when the recorded comment is
    /// in the fetched thread, so execution never depends on the plan
    /// surviving session compaction. An id outside the thread (fetch
    /// cap, deletion) degrades to the id-only reference.
    #[test]
    fn dispatch_embeds_the_recorded_plan_body() {
        let plan = Comment {
            id: "plan-77".into(),
            body: "## The Plan\n1. do the thing".into(),
            created_at: "2026-07-05T12:10:00Z".into(),
            user: Some(user("bot", "Kitaebot", "bot@example.com")),
        };
        let mut issues = [issue(
            "MDK-1",
            &["owner/repo"],
            vec![
                plan,
                comment(
                    "2026-07-05T12:30:00Z",
                    Some(user("u2", "Alice", TRUSTED)),
                    "LGTM, proceed",
                ),
            ],
        )];
        let mut st = state("2026-07-05T12:00:00Z", &["MDK-1"]);
        st.plan_comments.insert("MDK-1".into(), "plan-77".into());

        let (dispatches, next) = decide_events(&issues, &st, "bot", &trusted(), "needs-plan", NOW);
        let msg = &dispatches[0].message;
        assert!(msg.contains("Your current plan (comment plan-77)"), "{msg}");
        assert!(msg.contains("1. do the thing"), "{msg}");
        // The id survives into the next state while the issue is open.
        assert_eq!(
            next.plan_comments.get("MDK-1").map(String::as_str),
            Some("plan-77")
        );

        // Same state, plan comment missing from the thread: id only.
        issues[0].comments.nodes.remove(0);
        let (dispatches, _) = decide_events(&issues, &st, "bot", &trusted(), "needs-plan", NOW);
        let msg = &dispatches[0].message;
        assert!(!msg.contains("Your current plan"), "{msg}");
        assert!(msg.contains("(comment plan-77)"), "{msg}");
    }

    #[test]
    fn plan_comments_are_pruned_with_their_issues() {
        let issues = [issue("MDK-2", &["owner/repo"], vec![])];
        let mut st = state("2026-07-05T12:00:00Z", &["MDK-1", "MDK-2"]);
        st.plan_comments.insert("MDK-1".into(), "plan-77".into());
        st.plan_comments.insert("MDK-2".into(), "plan-88".into());

        let (_, next) = decide_events(&issues, &st, "bot", &trusted(), "needs-plan", NOW);

        assert_eq!(next.plan_comments.get("MDK-1"), None);
        assert_eq!(
            next.plan_comments.get("MDK-2").map(String::as_str),
            Some("plan-88")
        );
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

        let (dispatches, _) = decide_events(&issues, &st, "bot", &trusted(), "needs-plan", NOW);
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

        let (dispatches, _) = decide_events(&issues, &st, "bot", &trusted(), "needs-plan", NOW);
        assert!(dispatches.is_empty());
    }

    #[test]
    fn issue_without_repo_label_is_skipped_entirely() {
        let issues = [issue("MDK-1", &["bug"], vec![])];
        let st = state("2026-07-05T12:00:00Z", &[]);

        let (dispatches, next) = decide_events(&issues, &st, "bot", &trusted(), "needs-plan", NOW);
        assert!(dispatches.is_empty());
        // Not added to state: announced once the label shows up.
        assert!(!next.announced_issues.contains("MDK-1"));
    }

    #[test]
    fn issue_with_ambiguous_repo_labels_is_skipped() {
        let issues = [issue("MDK-1", &["owner/repo", "other/repo"], vec![])];
        let st = state("2026-07-05T12:00:00Z", &[]);

        let (dispatches, next) = decide_events(&issues, &st, "bot", &trusted(), "needs-plan", NOW);
        assert!(dispatches.is_empty());
        assert!(next.announced_issues.is_empty());
    }

    #[test]
    fn non_repo_labels_are_ignored() {
        let issues = [issue("MDK-1", &["bug", "owner/repo", "p0"], vec![])];
        let st = state("2026-07-05T12:00:00Z", &[]);

        let (dispatches, _) = decide_events(&issues, &st, "bot", &trusted(), "needs-plan", NOW);
        assert_eq!(dispatches.len(), 1);
        assert_eq!(dispatches[0].repo, "owner/repo");
        assert!(dispatches[0].message.contains("repo: owner/repo"));
    }

    #[test]
    fn vanished_issues_are_pruned_from_state() {
        let issues = [issue("MDK-2", &["owner/repo"], vec![])];
        let st = state("2026-07-05T12:00:00Z", &["MDK-1", "MDK-2"]);

        let (_, next) = decide_events(&issues, &st, "bot", &trusted(), "needs-plan", NOW);
        assert!(!next.announced_issues.contains("MDK-1"));
        assert!(next.announced_issues.contains("MDK-2"));
    }

    #[test]
    fn state_round_trip() {
        let db = crate::state_db::StateDb::open_in_memory().unwrap();

        let mut st = state("2026-07-05T12:00:00Z", &["MDK-1"]);
        st.plan_comments.insert("MDK-1".into(), "plan-77".into());
        save_state(&db, &st);
        let loaded = load_state(&db);
        assert_eq!(loaded.last_poll, "2026-07-05T12:00:00Z");
        assert!(loaded.announced_issues.contains("MDK-1"));
        assert_eq!(
            loaded.plan_comments.get("MDK-1").map(String::as_str),
            Some("plan-77")
        );
    }

    #[test]
    fn state_without_plan_comments_still_loads() {
        // Deployed state predates the field; serde default must cover it.
        let db = crate::state_db::StateDb::open_in_memory().unwrap();
        db.put_doc(
            DOC,
            r#"{"last_poll":"2026-07-05T12:00:00Z","announced_issues":["MDK-1"]}"#,
        )
        .unwrap();

        let loaded = load_state(&db);

        assert!(loaded.announced_issues.contains("MDK-1"));
        assert!(loaded.plan_comments.is_empty());
    }

    #[test]
    fn load_missing_or_corrupt_state_starts_now() {
        let db = crate::state_db::StateDb::open_in_memory().unwrap();

        let missing = load_state(&db);
        assert!(missing.last_poll.ends_with('Z'));
        assert!(missing.announced_issues.is_empty());

        db.put_doc("linear_poll", "not json").unwrap();
        let corrupt = load_state(&db);
        assert!(corrupt.last_poll.ends_with('Z'));
        assert!(corrupt.announced_issues.is_empty());
    }

    // -- Shell tests --

    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use crate::clients::RawResponse;

    /// Fake client: captures `commentCreate` bodies, pops queued results.
    fn comment_client(
        results: Vec<Result<(), LinearError>>,
        sent: Arc<Mutex<Vec<String>>>,
    ) -> LinearClient {
        let results = Arc::new(Mutex::new(VecDeque::from(results)));
        LinearClient::from_fn(move |body| {
            let results = Arc::clone(&results);
            let sent = Arc::clone(&sent);
            async move {
                let req: serde_json::Value = serde_json::from_slice(&body).unwrap();
                sent.lock()
                    .unwrap()
                    .push(req["variables"]["body"].as_str().unwrap().to_string());
                match results
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pop_front()
                    .unwrap()
                {
                    Ok(()) => Ok(RawResponse {
                        status: 200,
                        body:
                            br#"{"data":{"commentCreate":{"success":true,"comment":{"id":"c-9"}}}}"#
                                .to_vec(),
                        retry_after_secs: None,
                    }),
                    Err(e) => Err(e),
                }
            }
        })
    }

    fn channel(client: LinearClient) -> LinearChannel {
        LinearChannel::new(
            client,
            Duration::from_mins(2),
            trusted(),
            "needs-plan".into(),
            None,
        )
    }

    #[test]
    fn transient_error_classification() {
        assert!(is_transient(&LinearError::Network("timeout".into())));
        assert!(!is_transient(&LinearError::Api("bad input".into())));
        assert!(!is_transient(&LinearError::Deserialize("nope".into())));
    }

    #[tokio::test]
    async fn post_comment_retries_transient_then_succeeds() {
        tokio::time::pause();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let ch = channel(comment_client(
            vec![
                Err(LinearError::Network("timeout".into())),
                Err(LinearError::Network("503".into())),
                Ok(()),
            ],
            sent.clone(),
        ));

        ch.post_comment("i1", "plan").await.unwrap();

        assert_eq!(
            sent.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            3
        );
    }

    #[tokio::test]
    async fn post_comment_does_not_retry_permanent_error() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let ch = channel(comment_client(
            vec![Err(LinearError::Api("invalid issue".into()))],
            sent.clone(),
        ));

        let err = ch.post_comment("i1", "plan").await.unwrap_err();

        assert_eq!(
            sent.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );
        assert!(matches!(err, LinearError::Api(_)));
    }

    #[tokio::test]
    async fn dispatch_posts_reply_as_comment() {
        use crate::provider::MockProvider;
        use crate::test_support::{TestAgent, workspace};
        use crate::types::Response;

        let (_dir, ws) = workspace();
        let provider = Arc::new(MockProvider::new(vec![Ok(Response::Text("a plan".into()))]));
        let handle = TestAgent::new(ws, provider).spawn();

        let sent = Arc::new(Mutex::new(Vec::new()));
        let ch = channel(comment_client(vec![Ok(())], sent.clone()));

        let plan_id = dispatch(
            &ch,
            &handle,
            Dispatch {
                issue_id: "i1".into(),
                identifier: "MDK-1".into(),
                repo: "owner/repo".into(),
                message: "new issue".into(),
                kind: TurnKind::Plan,
            },
        )
        .await;

        let sent = sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0], "a plan");
        // The announcement reply is the plan; its id gets recorded.
        assert_eq!(plan_id.as_deref(), Some("c-9"));
    }

    #[tokio::test]
    async fn checkout_note_reflects_need_and_missing_git() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let ch = channel(comment_client(vec![], sent));

        let plan = Dispatch {
            issue_id: "i1".into(),
            identifier: "MDK-1".into(),
            repo: "owner/repo".into(),
            message: "plan".into(),
            kind: TurnKind::Plan,
        };
        assert!(checkout_note(&ch, &plan).await.is_none());

        let exec = Dispatch {
            kind: TurnKind::Execution,
            ..plan
        };
        // No git wired: the agent is told to clone for itself.
        assert_eq!(
            checkout_note(&ch, &exec).await.as_deref(),
            Some(execution_checkout::CLONE_YOURSELF)
        );
    }
}
