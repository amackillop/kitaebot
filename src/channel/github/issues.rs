//! GitHub issue polling channel.
//!
//! Polls for open issues assigned to the bot account in the configured
//! repositories. New issues are announced to the agent, which replies
//! with an implementation plan; comments from trusted users drive plan
//! revision or end-to-end execution. Replies are posted back as issue
//! comments. Assignment is the human gate for *work*: an issue nobody
//! assigned to the bot — its own included — dispatches no execution.
//!
//! A second, discussion-only pass covers unassigned issues: trusted
//! comments on bot-authored issues (open, or recently closed — the
//! disposition case) and on issues where a trusted user mentioned the
//! bot. Discussion turns reply in the thread and prepare no checkout;
//! settling a direction there is exactly what precedes assignment.
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

use super::trust::Trust;
use crate::agent::AgentHandle;
use crate::agent::envelope::{ChannelSource, TurnRole};
use crate::channel::execution_checkout;
use crate::clients::github::{GithubClient, IssueComment, SearchIssue};
use crate::config::GithubConfig;
use crate::error::GithubError;
use crate::state_db::StateDb;
use crate::time::now_iso8601;
use crate::tools::git::GitCli;

/// Maximum retries for posting a reply comment on transient failures.
const POST_RETRIES: u32 = 3;

/// Whether a [`GithubError`] is worth retrying. Rate limits are: the
/// client gate holds every request until the server-mandated cooldown
/// passes, so the retry waits exactly as long as GitHub asked.
fn is_transient(err: &GithubError) -> bool {
    match err {
        GithubError::Api { status, .. } => (500..=599).contains(status),
        GithubError::Deserialize(_) => false,
        GithubError::Network(_) | GithubError::RateLimited { .. } => true,
    }
}

// ---------------------------------------------------------------------------
// Poll loop
// ---------------------------------------------------------------------------

/// Run the GitHub issue polling loop forever.
///
/// Resolves the bot login once at startup; failure disables the channel
/// (logged, then pending forever) rather than crashing the daemon.
pub async fn poll_loop(
    client: &GithubClient,
    git: &GitCli,
    config: &GithubConfig,
    repos: &[String],
    handle: &AgentHandle,
    state_db: &StateDb,
) -> ! {
    let bot_login = match client.user().await {
        Ok(user) => {
            info!(login = %user.login, "GitHub issues channel resolved bot identity");
            user.login
        }
        Err(e) => {
            error!("GitHub issues channel: failed to resolve bot login: {e}");
            std::future::pending().await
        }
    };

    let mut state = load_state(state_db);
    info!(last_poll = %state.last_poll, "GitHub issues channel starting");

    let mut tick = time::interval(Duration::from_secs(config.issues.poll_interval_secs));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tick.tick().await;
        let views = match fetch_views(client, &bot_login, repos, &state).await {
            Ok(views) => views,
            Err(e) => {
                error!("GitHub issues poll error (will retry next tick): {e}");
                continue;
            }
        };

        let (dispatches, next) = decide_events(
            &views,
            &state,
            &bot_login,
            &Trust::new(config),
            &config.issues.plan_label,
            &now_iso8601(),
        );
        let count = dispatches.len();
        let mut next = next;
        for d in dispatches {
            let key = d.key.clone();
            if let Some(plan_id) = dispatch(client, git, handle, d).await {
                next.plan_comments.insert(key, plan_id);
            }
        }
        info!(count, "GitHub issues poll: dispatched {count} items");

        state = next;
        save_state(state_db, &state);
    }
}

/// Fetch the tick's issues and the comments of the ones that need
/// them: the work search (assigned) plus the discussion searches
/// (bot-authored open, bot-authored recently closed, and mentions).
/// The discussion searches are cursor-bounded — a new comment is the
/// only trigger and it bumps `updated_at`, so untouched issues never
/// surface. An issue found by more than one search keeps its first
/// view; the work search runs first, so assignment wins.
async fn fetch_views(
    client: &GithubClient,
    bot_login: &str,
    repos: &[String],
    state: &PollState,
) -> Result<Vec<IssueView>, GithubError> {
    let cursor = &state.last_poll;
    let searches = [
        (
            format!("is:issue is:open assignee:{bot_login}"),
            ViewMode::Work,
        ),
        (
            format!("is:issue is:open author:{bot_login} -assignee:{bot_login} updated:>{cursor}"),
            ViewMode::Discussion { closed: false },
        ),
        (
            format!("is:issue is:closed author:{bot_login} updated:>{cursor}"),
            ViewMode::Discussion { closed: true },
        ),
        (
            format!(
                "is:issue is:open mentions:{bot_login} -assignee:{bot_login} \
                 -author:{bot_login} updated:>{cursor}"
            ),
            ViewMode::Discussion { closed: false },
        ),
    ];

    let mut views: Vec<IssueView> = Vec::new();
    let mut seen = BTreeSet::new();
    for (query, mode) in searches {
        for issue in client.search_issues(&query).await? {
            let Some(nwo) = issue.nwo() else {
                warn!(
                    number = issue.number,
                    "Skipping issue with unparseable repository URL"
                );
                continue;
            };
            if !repos.contains(&nwo) {
                if mode == ViewMode::Work {
                    warn!(
                        issue = %format!("{nwo}#{}", issue.number),
                        "Skipping issue in unconfigured repository"
                    );
                }
                continue;
            }
            if !seen.insert(format!("{nwo}#{}", issue.number)) {
                continue;
            }
            let comments = if wants_comments(&issue, &nwo, state, mode) {
                client.issue_comments(&nwo, issue.number).await?
            } else {
                Vec::new()
            };
            views.push(IssueView {
                issue,
                nwo,
                comments,
                mode,
            });
        }
    }
    Ok(views)
}

/// Prepare a fresh base checkout for an execution turn and describe it
/// for the agent, or `None` when the turn needs no checkout.
async fn checkout_note(git: &GitCli, d: &Dispatch) -> Option<String> {
    if d.kind != TurnKind::Execution {
        return None;
    }
    match execution_checkout::prepare(git, &d.nwo).await {
        Ok(prepared) => Some(prepared.ready_note()),
        Err(e) => {
            warn!(issue = %d.key, "execution checkout prep failed: {e}");
            Some(execution_checkout::CLONE_YOURSELF.into())
        }
    }
}

/// Run one agent turn and post the reply (or error) as a comment.
/// Returns the posted comment's id for announcement turns — that
/// comment is the plan, and revision turns need its id.
async fn dispatch(
    client: &GithubClient,
    git: &GitCli,
    handle: &AgentHandle,
    d: Dispatch,
) -> Option<u64> {
    let cancel = CancellationToken::new();
    let source = ChannelSource::GitHubIssue {
        issue: d.key.clone(),
    };
    let message = match checkout_note(git, &d).await {
        Some(note) => format!("{}\n\n{note}", d.message),
        None => d.message.clone(),
    };
    // Route per-repo: all of a repo's tickets — and its PRs, which use
    // the same key — share one session.
    let body = match handle
        .send_message_with_role(
            source,
            message,
            Some(d.nwo.clone()),
            None,
            cancel,
            // Plan turns think on the planner override (spec 25);
            // execution and discussion ride the default, including
            // post-plan revision comments — a revised plan still
            // passes the plan gate.
            match d.kind {
                TurnKind::Plan => TurnRole::Planner,
                TurnKind::Discussion | TurnKind::Execution => TurnRole::Default,
            },
        )
        .await
    {
        Ok(reply) => {
            info!("GitHub issue {}: {}", d.key, reply.content);
            reply.content
        }
        Err(e) => {
            error!("GitHub issue {} error: {e}", d.key);
            e
        }
    };
    match post_comment(client, &d.nwo, d.number, &body).await {
        Ok(posted) => (d.kind == TurnKind::Plan).then_some(posted.id),
        Err(e) => {
            error!("GitHub issue {}: failed to post comment: {e}", d.key);
            None
        }
    }
}

/// Post a comment with retries on transient failures.
///
/// Retries up to [`POST_RETRIES`] times with exponential backoff
/// (1s, 2s, 4s) on network errors, 429, and 5xx.
async fn post_comment(
    client: &GithubClient,
    nwo: &str,
    number: u32,
    body: &str,
) -> Result<IssueComment, GithubError> {
    let mut attempts = 0u32;
    loop {
        match client.create_issue_comment(nwo, number, body).await {
            Ok(posted) => return Ok(posted),
            Err(e) if attempts < POST_RETRIES && is_transient(&e) => {
                let delay = Duration::from_secs(u64::from(1u32 << attempts));
                attempts += 1;
                warn!(
                    attempt = attempts,
                    "create_issue_comment retrying in {delay:?}: {e}"
                );
                time::sleep(delay).await;
            }
            Err(e) => return Err(e),
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
    /// Issues already announced to the agent, keyed `owner/repo#42`.
    pub announced_issues: BTreeSet<String>,
    /// The bot's plan comment per issue, keyed `owner/repo#42` — the
    /// announcement reply's comment id, handed back to revision turns
    /// so the plan can be edited in place.
    #[serde(default)]
    pub plan_comments: BTreeMap<String, u64>,
    /// Issues whose discussion thread was already embedded in a turn,
    /// keyed `owner/repo#42`; later comments dispatch incrementally.
    #[serde(default)]
    pub discussion_announced: BTreeSet<String>,
}

impl PollState {
    /// Fresh state: announce assigned issues, replay no comments.
    fn starting_now() -> Self {
        Self {
            last_poll: now_iso8601(),
            announced_issues: BTreeSet::new(),
            plan_comments: BTreeMap::new(),
            discussion_announced: BTreeSet::new(),
        }
    }
}

const DOC: &str = "github_issues_poll";

pub fn load_state(db: &StateDb) -> PollState {
    db.load_json(DOC, || {
        info!("No GitHub issues poll state, starting from now");
        PollState::starting_now()
    })
}

pub fn save_state(db: &StateDb, state: &PollState) {
    db.save_json(DOC, state);
}

// ---------------------------------------------------------------------------
// Event detection (pure core)
// ---------------------------------------------------------------------------

/// How an issue reached the poll. Decides the choreography: work views
/// get the plan/execute machinery, discussion views get a reply-only
/// turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewMode {
    /// Unassigned issue surfaced for peer discussion. `closed` is read
    /// from the search that found it: comments on a closed bot-authored
    /// issue are disposition on finished work, and the prompt says so.
    Discussion { closed: bool },
    /// Assigned to the bot: the ticket is a work item.
    Work,
}

/// A polled issue with its routing key and (possibly skipped)
/// comment fetch resolved.
pub struct IssueView {
    pub issue: SearchIssue,
    /// `owner/repo`, parsed from the repository URL.
    pub nwo: String,
    pub comments: Vec<IssueComment>,
    pub mode: ViewMode,
}

impl IssueView {
    /// Tracking key, `owner/repo#42`.
    fn key(&self) -> String {
        format!("{}#{}", self.nwo, self.issue.number)
    }
}

/// Whether an issue's comments must be fetched this tick. Work views:
/// always for unannounced issues (the announcement embeds them),
/// otherwise only when the issue changed since the cursor. Discussion
/// views need comments only when the issue changed — a new comment is
/// the only trigger, and it bumps `updated_at`.
pub fn wants_comments(issue: &SearchIssue, nwo: &str, state: &PollState, mode: ViewMode) -> bool {
    let changed = issue.updated_at.as_str() > state.last_poll.as_str();
    match mode {
        ViewMode::Discussion { .. } => changed,
        ViewMode::Work => {
            changed
                || !state
                    .announced_issues
                    .contains(&format!("{nwo}#{}", issue.number))
        }
    }
}

/// What a dispatched turn is for; each kind gets different plumbing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnKind {
    /// Peer discussion: no checkout, reply is just a comment.
    Discussion,
    /// May implement: a fresh base checkout is prepared first.
    Execution,
    /// Plan-first announcement: the reply comment id is recorded so
    /// revision turns can edit the plan in place.
    Plan,
}

/// One agent turn to run: message in, reply posted as a comment.
#[derive(Debug)]
pub struct Dispatch {
    /// Tracking key, `owner/repo#42`.
    pub key: String,
    /// `owner/repo` — the session routing key, shared with the PR
    /// channel so a repo's PRs and tickets land in the same session.
    pub nwo: String,
    pub number: u32,
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
    views: &[IssueView],
    state: &PollState,
    bot_login: &str,
    trust: &Trust,
    plan_label: &str,
    now: &str,
) -> (Vec<Dispatch>, PollState) {
    let mut dispatches = Vec::new();
    let mut announced = BTreeSet::new();
    let mut discussion_announced = BTreeSet::new();

    // Work views first: when a race lands an issue in both passes the
    // same tick, assignment wins and the discussion view is dropped.
    let (work, discussion): (Vec<&IssueView>, Vec<&IssueView>) =
        views.iter().partition(|v| matches!(v.mode, ViewMode::Work));

    for view in work {
        let key = view.key();
        if !state.announced_issues.contains(&key) {
            // The label chooses the choreography: plan-first when the
            // human asked for one, direct execution otherwise.
            let plan_first = view.issue.has_label(plan_label);
            dispatches.push(Dispatch {
                key: key.clone(),
                nwo: view.nwo.clone(),
                number: view.issue.number,
                message: if plan_first {
                    format_new_issue(view, trust, bot_login)
                } else {
                    format_new_issue_execute(view, trust, bot_login)
                },
                kind: if plan_first {
                    TurnKind::Plan
                } else {
                    TurnKind::Execution
                },
            });
            announced.insert(key);
            continue;
        }
        announced.insert(key.clone());

        for comment in new_trusted_comments(view, state, bot_login, trust) {
            dispatches.push(Dispatch {
                key: key.clone(),
                nwo: view.nwo.clone(),
                number: view.issue.number,
                message: format_comment(
                    view,
                    &comment.user.login,
                    &comment.body,
                    state.plan_comments.get(&key).copied(),
                ),
                kind: TurnKind::Execution,
            });
        }
    }

    for view in discussion {
        let key = view.key();
        if announced.contains(&key) {
            continue;
        }
        let closed = matches!(view.mode, ViewMode::Discussion { closed: true });
        let new_trusted = new_trusted_comments(view, state, bot_login, trust);
        if new_trusted.is_empty() {
            // Membership persists while the issue stays in view, so
            // follow-up comments dispatch incrementally.
            if state.discussion_announced.contains(&key) {
                discussion_announced.insert(key);
            }
            continue;
        }
        if state.discussion_announced.contains(&key) {
            for comment in new_trusted {
                dispatches.push(Dispatch {
                    key: key.clone(),
                    nwo: view.nwo.clone(),
                    number: view.issue.number,
                    message: format_discussion_comment(
                        view,
                        &comment.user.login,
                        &comment.body,
                        closed,
                    ),
                    kind: TurnKind::Discussion,
                });
            }
        } else {
            // First discussion turn embeds the full trusted thread;
            // the new comments are part of it.
            dispatches.push(Dispatch {
                key: key.clone(),
                nwo: view.nwo.clone(),
                number: view.issue.number,
                message: format_discussion(view, trust, bot_login, closed),
                kind: TurnKind::Discussion,
            });
        }
        discussion_announced.insert(key);
    }

    let next = PollState {
        last_poll: now.to_string(),
        // Keys absent from the fetch (closed, unassigned) are pruned
        // by rebuilding from fetched issues only; plan ids follow the
        // same lifetime.
        plan_comments: state
            .plan_comments
            .iter()
            .filter(|(k, _)| announced.contains(*k))
            .map(|(k, v)| (k.clone(), *v))
            .collect(),
        announced_issues: announced,
        discussion_announced,
    };
    (dispatches, next)
}

/// Comments past the cursor from trusted users other than the bot,
/// with untrusted skips logged.
fn new_trusted_comments<'a>(
    view: &'a IssueView,
    state: &PollState,
    bot_login: &str,
    trust: &Trust,
) -> Vec<&'a IssueComment> {
    view.comments
        .iter()
        .filter(|c| c.created_at.as_str() > state.last_poll.as_str())
        .filter(|c| c.user.login != bot_login)
        .filter(|c| {
            if !trust.allows(&c.user.login) {
                warn!(
                    issue = %view.key(),
                    author = %c.user.login,
                    "Skipping comment from untrusted user"
                );
                return false;
            }
            true
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Message formatting
// ---------------------------------------------------------------------------

/// Filter comments to those from trusted users or the bot itself,
/// logging skips at the same level as the post-assignment filter so
/// dropped context is visible in the journal. The bot's own comments
/// (plan posts) are kept so the announcement carries the bot's prior
/// work in the thread.
fn trusted_comments<'a>(
    comments: &'a [IssueComment],
    trust: &Trust,
    bot_login: &str,
    key: &str,
) -> Vec<&'a IssueComment> {
    comments
        .iter()
        .filter(|c| {
            if c.user.login == bot_login {
                return true;
            }
            if !trust.allows(&c.user.login) {
                warn!(
                    issue = %key,
                    author = %c.user.login,
                    "Skipping comment from untrusted user in announcement"
                );
                return false;
            }
            true
        })
        .collect()
}

fn format_new_issue(view: &IssueView, trust: &Trust, bot_login: &str) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "GitHub issue {} \"{}\" was assigned to you.",
        view.key(),
        view.issue.title,
    );
    if let Some(body) = view.issue.body.as_deref().filter(|b| !b.is_empty()) {
        let _ = writeln!(s, "\nDescription:\n{body}");
    }
    let trusted = trusted_comments(&view.comments, trust, bot_login, &view.key());
    if !trusted.is_empty() {
        let _ = writeln!(s, "\nExisting comments:");
        for comment in &trusted {
            let _ = writeln!(s, "[{}] {}", comment.user.login, comment.body);
        }
    }
    let _ = writeln!(s, "\n{}", crate::channel::PLAN_INSTRUCTIONS);
    s
}

/// The direct-execution announcement, for issues assigned without
/// the plan label.
fn format_new_issue_execute(view: &IssueView, trust: &Trust, bot_login: &str) -> String {
    let branch = format!("kitaebot_issue-{}_<short-summary>", view.issue.number);
    let number = view.issue.number;
    let mut s = String::new();
    let _ = writeln!(
        s,
        "GitHub issue {} \"{}\" was assigned to you for direct execution \
         (no plan requested).",
        view.key(),
        view.issue.title,
    );
    if let Some(body) = view.issue.body.as_deref().filter(|b| !b.is_empty()) {
        let _ = writeln!(s, "\nDescription:\n{body}");
    }
    let trusted = trusted_comments(&view.comments, trust, bot_login, &view.key());
    if !trusted.is_empty() {
        let _ = writeln!(s, "\nExisting comments:");
        for comment in &trusted {
            let _ = writeln!(s, "[{}] {}", comment.user.login, comment.body);
        }
    }
    let _ = writeln!(
        s,
        "\nImplement it end-to-end: create a branch named {branch}, \
         implement, test, commit, push, and open a PR whose description \
         includes \"Closes #{number}\" so merging it closes this issue. \
         On success reply with one line at most; the PR cross-references \
         itself on the ticket. Be detailed only if something failed or \
         needs a decision. If the ticket turns out underspecified or \
         materially larger than it reads, stop before implementing and \
         reply with your plan or questions instead — your reply is posted \
         verbatim as a comment on the ticket."
    );
    s
}

fn format_comment(view: &IssueView, author: &str, body: &str, plan_comment: Option<u64>) -> String {
    let branch = format!("kitaebot_issue-{}_<short-summary>", view.issue.number);
    let number = view.issue.number;
    let mut s = String::new();
    let _ = writeln!(
        s,
        "Comment on GitHub issue {} \"{}\" by @{author}:",
        view.key(),
        view.issue.title,
    );
    let _ = writeln!(s, "\n{body}");
    let _ = writeln!(
        s,
        "\nIf this approves your plan, execute it end-to-end: create a \
         branch named {branch}, implement, test, commit, push, and open a \
         PR whose description includes \"Closes #{number}\" so merging it \
         closes this issue. On success reply with one line at most; the PR \
         cross-references itself on the ticket. Be detailed only if \
         something failed or needs a decision."
    );
    match plan_comment {
        Some(id) => {
            let _ = writeln!(
                s,
                "\nIf the comment is feedback on the plan instead, engage \
                 with it like a colleague — your reply is posted verbatim \
                 as a comment, and some prose discussing the request is \
                 welcome. Where the feedback improves the plan, revise the \
                 plan in place with github_comment_update (your plan is \
                 comment id {id}; the edit history shows the reviewer what \
                 changed) and summarize the change in your reply. Where you \
                 disagree, push back with your reasoning and leave the plan \
                 unchanged on that point — do not adopt changes you believe \
                 are wrong just to comply."
            );
        }
        None => {
            let _ = writeln!(
                s,
                "\nIf the comment is feedback instead, revise your plan and \
                 reply with the updated plan."
            );
        }
    }
    s
}

/// The standing instructions for a discussion turn, varied by issue
/// state: open issues are direction-setting, closed ones disposition.
fn discussion_instructions(closed: bool) -> &'static str {
    if closed {
        "This issue is closed, so the comments are disposition on \
         finished work — how a human resolved or judged it. Take note \
         of anything that should change how you work, and reply \
         briefly; your reply is posted verbatim as a comment. Do not \
         reopen the issue or start any work."
    } else {
        "This is discussion, not an assignment. Reply as a peer: \
         engage with the comments, answer questions, and where a \
         comment changes your mind about the issue's direction, say so \
         concretely — your reply is posted verbatim as a comment on \
         the issue. Where you disagree, push back with your reasoning \
         rather than complying. Do not create branches, commit, open \
         PRs, or start implementing; assignment is the gate for work. \
         If the discussion settles the direction, state it plainly so \
         a later assignment starts from it."
    }
}

/// The first discussion turn on an issue: full context, since the
/// session may have long since compacted the issue away.
fn format_discussion(view: &IssueView, trust: &Trust, bot_login: &str, closed: bool) -> String {
    let state_word = if closed { "closed " } else { "" };
    let mut s = String::new();
    let _ = writeln!(
        s,
        "New discussion on {state_word}GitHub issue {} \"{}\" (not assigned to you).",
        view.key(),
        view.issue.title,
    );
    if let Some(body) = view.issue.body.as_deref().filter(|b| !b.is_empty()) {
        let _ = writeln!(s, "\nDescription:\n{body}");
    }
    let trusted = trusted_comments(&view.comments, trust, bot_login, &view.key());
    if !trusted.is_empty() {
        let _ = writeln!(s, "\nComments:");
        for comment in &trusted {
            let _ = writeln!(s, "[{}] {}", comment.user.login, comment.body);
        }
    }
    let _ = writeln!(s, "\n{}", discussion_instructions(closed));
    s
}

/// A follow-up comment on an already-discussed issue.
fn format_discussion_comment(view: &IssueView, author: &str, body: &str, closed: bool) -> String {
    let state_word = if closed { "closed " } else { "" };
    let mut s = String::new();
    let _ = writeln!(
        s,
        "Comment on {state_word}GitHub issue {} \"{}\" by @{author} \
         (discussion; not assigned to you):",
        view.key(),
        view.issue.title,
    );
    let _ = writeln!(s, "\n{body}");
    let _ = writeln!(s, "\n{}", discussion_instructions(closed));
    s
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::github::UserRef;

    fn comment(created_at: &str, login: &str, body: &str) -> IssueComment {
        IssueComment {
            id: 1,
            user: UserRef {
                login: login.into(),
            },
            body: body.into(),
            created_at: created_at.into(),
        }
    }

    fn labeled_view(
        number: u32,
        updated_at: &str,
        comments: Vec<IssueComment>,
        labels: &[&str],
    ) -> IssueView {
        IssueView {
            issue: SearchIssue {
                number,
                title: "Fix login".into(),
                body: Some("It is broken".into()),
                user: UserRef {
                    login: "alice".into(),
                },
                repository_url: "https://api.github.com/repos/owner/repo".into(),
                updated_at: updated_at.into(),
                labels: labels
                    .iter()
                    .map(|n| crate::clients::github::IssueLabel { name: (*n).into() })
                    .collect(),
            },
            nwo: "owner/repo".into(),
            comments,
            mode: ViewMode::Work,
        }
    }

    /// A view carrying the plan label — most tests exercise the
    /// plan-first choreography.
    fn view(number: u32, updated_at: &str, comments: Vec<IssueComment>) -> IssueView {
        labeled_view(number, updated_at, comments, &["needs-plan"])
    }

    /// An unassigned view surfaced by the discussion pass.
    fn discussion_view(number: u32, closed: bool, comments: Vec<IssueComment>) -> IssueView {
        let mut v = labeled_view(number, "2026-08-04T12:30:00Z", comments, &[]);
        v.mode = ViewMode::Discussion { closed };
        v
    }

    fn state(last_poll: &str, announced: &[&str]) -> PollState {
        PollState {
            last_poll: last_poll.into(),
            announced_issues: announced.iter().map(|s| (*s).into()).collect(),
            plan_comments: BTreeMap::new(),
            discussion_announced: BTreeSet::new(),
        }
    }

    const NOW: &str = "2026-08-04T13:00:00Z";
    const BOT: &str = "kitaebot";

    fn config() -> crate::config::GithubConfig {
        crate::config::GithubConfig {
            owner: "boss".into(),
            trusted_users: vec!["alice".into()],
            ..Default::default()
        }
    }

    fn decide(views: &[IssueView], st: &PollState) -> (Vec<Dispatch>, PollState) {
        let config = config();
        decide_events(views, st, BOT, &Trust::new(&config), "needs-plan", NOW)
    }

    #[test]
    fn new_issue_is_announced_once() {
        let views = [view(1, "2026-08-04T12:30:00Z", vec![])];
        let st = state("2026-08-04T12:00:00Z", &[]);

        let (dispatches, next) = decide(&views, &st);
        assert_eq!(dispatches.len(), 1);
        assert_eq!(dispatches[0].key, "owner/repo#1");
        assert_eq!(dispatches[0].nwo, "owner/repo");
        assert_eq!(dispatches[0].number, 1);
        assert!(dispatches[0].message.contains("assigned to you"));
        assert_eq!(dispatches[0].kind, TurnKind::Plan);
        assert!(next.announced_issues.contains("owner/repo#1"));
        assert_eq!(next.last_poll, NOW);

        // Second tick: already announced, no new comments — nothing.
        let (dispatches, _) = decide(&views, &next);
        assert!(dispatches.is_empty());
    }

    #[test]
    fn announcement_embeds_description_and_comments() {
        let views = [view(
            1,
            "2026-08-04T12:30:00Z",
            vec![comment(
                "2026-08-04T11:00:00Z",
                "alice",
                "please prioritize",
            )],
        )];
        let st = state("2026-08-04T12:00:00Z", &[]);

        let (dispatches, _) = decide(&views, &st);
        assert_eq!(dispatches.len(), 1);
        let msg = &dispatches[0].message;
        assert!(msg.contains("It is broken"));
        assert!(msg.contains("[alice] please prioritize"));
        assert!(msg.contains("Do not implement anything yet"));
        assert!(msg.contains("posted verbatim"));
    }

    #[test]
    fn announcement_filters_untrusted_comments() {
        let views = [view(
            1,
            "2026-08-04T12:30:00Z",
            vec![
                comment("2026-08-04T11:00:00Z", "alice", "trusted guidance"),
                comment("2026-08-04T11:30:00Z", "mallory", "untrusted noise"),
                comment("2026-08-04T11:45:00Z", BOT, "my own plan post"),
            ],
        )];
        let st = state("2026-08-04T12:00:00Z", &[]);

        let (dispatches, _) = decide(&views, &st);
        assert_eq!(dispatches.len(), 1);
        let msg = &dispatches[0].message;
        // Trusted user's comment is embedded.
        assert!(msg.contains("[alice] trusted guidance"));
        // Bot's own comment is embedded (plan posts carry forward).
        assert!(msg.contains("[kitaebot] my own plan post"));
        // Untrusted user's comment is absent.
        assert!(!msg.contains("mallory"));
        assert!(!msg.contains("untrusted noise"));
    }

    #[test]
    fn announcement_filters_untrusted_comments_execute() {
        let views = [labeled_view(
            1,
            "2026-08-04T12:30:00Z",
            vec![
                comment("2026-08-04T11:00:00Z", "boss", "owner says go"),
                comment("2026-08-04T11:30:00Z", "mallory", "inject this"),
            ],
            &["bug"],
        )];
        let st = state("2026-08-04T12:00:00Z", &[]);

        let (dispatches, _) = decide(&views, &st);
        assert_eq!(dispatches.len(), 1);
        let msg = &dispatches[0].message;
        assert!(msg.contains("[boss] owner says go"));
        assert!(!msg.contains("mallory"));
        assert!(!msg.contains("inject this"));
    }

    #[test]
    fn announcement_with_only_untrusted_comments_omits_section() {
        let views = [view(
            1,
            "2026-08-04T12:30:00Z",
            vec![comment("2026-08-04T11:00:00Z", "mallory", "evil")],
        )];
        let st = state("2026-08-04T12:00:00Z", &[]);

        let (dispatches, _) = decide(&views, &st);
        assert_eq!(dispatches.len(), 1);
        let msg = &dispatches[0].message;
        assert!(!msg.contains("Existing comments"));
        assert!(!msg.contains("mallory"));
    }

    #[test]
    fn announced_issue_skips_comment_pass_same_tick() {
        // A new comment newer than last_poll on a not-yet-announced issue
        // must not double dispatch — it rides along in the announcement.
        let views = [view(
            1,
            "2026-08-04T12:30:00Z",
            vec![comment("2026-08-04T12:30:00Z", "alice", "go")],
        )];
        let st = state("2026-08-04T12:00:00Z", &[]);

        let (dispatches, _) = decide(&views, &st);
        assert_eq!(dispatches.len(), 1);
        assert!(dispatches[0].message.contains("assigned to you"));
    }

    #[test]
    fn new_trusted_comment_dispatches() {
        let views = [view(
            1,
            "2026-08-04T12:30:00Z",
            vec![comment(
                "2026-08-04T12:30:00Z",
                "alice",
                "approved, go ahead",
            )],
        )];
        let st = state("2026-08-04T12:00:00Z", &["owner/repo#1"]);

        let (dispatches, _) = decide(&views, &st);
        assert_eq!(dispatches.len(), 1);
        assert_eq!(dispatches[0].nwo, "owner/repo");
        assert_eq!(dispatches[0].kind, TurnKind::Execution);
        let msg = &dispatches[0].message;
        assert!(msg.contains("approved, go ahead"));
        assert!(msg.contains("kitaebot_issue-1_<short-summary>"));
        assert!(msg.contains("Closes #1"));
        // No recorded plan comment: the fallback revision text applies.
        assert!(msg.contains("revise your plan and reply"));
    }

    #[test]
    fn unlabeled_issue_executes_directly() {
        let views = [labeled_view(1, "2026-08-04T12:30:00Z", vec![], &["bug"])];
        let st = state("2026-08-04T12:00:00Z", &[]);

        let (dispatches, next) = decide(&views, &st);

        assert_eq!(dispatches.len(), 1);
        assert_eq!(
            dispatches[0].kind,
            TurnKind::Execution,
            "direct execution needs a checkout"
        );
        let msg = &dispatches[0].message;
        assert!(msg.contains("direct execution"), "{msg}");
        assert!(msg.contains("Closes #1"));
        assert!(
            msg.contains("stop before implementing"),
            "needs the escape hatch"
        );
        assert!(!msg.contains("Do not implement anything yet"));
        assert!(next.announced_issues.contains("owner/repo#1"));
    }

    #[test]
    fn plan_label_is_case_insensitive() {
        let views = [labeled_view(
            1,
            "2026-08-04T12:30:00Z",
            vec![],
            &["Needs-Plan"],
        )];
        let st = state("2026-08-04T12:00:00Z", &[]);

        let (dispatches, _) = decide(&views, &st);

        assert_eq!(dispatches[0].kind, TurnKind::Plan);
        assert!(
            dispatches[0]
                .message
                .contains("Do not implement anything yet")
        );
    }

    #[test]
    fn known_plan_comment_enables_in_place_revision() {
        let views = [view(
            1,
            "2026-08-04T12:30:00Z",
            vec![comment("2026-08-04T12:30:00Z", "alice", "what about X?")],
        )];
        let mut st = state("2026-08-04T12:00:00Z", &["owner/repo#1"]);
        st.plan_comments.insert("owner/repo#1".into(), 77);

        let (dispatches, next) = decide(&views, &st);

        let msg = &dispatches[0].message;
        assert!(msg.contains("comment id 77"), "{msg}");
        assert!(msg.contains("github_comment_update"));
        assert!(msg.contains("disagree"), "revision must license pushback");
        // The id survives into the next state while the issue is open.
        assert_eq!(next.plan_comments.get("owner/repo#1"), Some(&77));
    }

    #[test]
    fn plan_comments_are_pruned_with_their_issues() {
        let views = [view(2, "2026-08-04T12:30:00Z", vec![])];
        let mut st = state("2026-08-04T12:00:00Z", &["owner/repo#1", "owner/repo#2"]);
        st.plan_comments.insert("owner/repo#1".into(), 77);
        st.plan_comments.insert("owner/repo#2".into(), 88);

        let (_, next) = decide(&views, &st);

        assert_eq!(next.plan_comments.get("owner/repo#1"), None);
        assert_eq!(next.plan_comments.get("owner/repo#2"), Some(&88));
    }

    #[test]
    fn owner_comment_is_trusted() {
        let views = [view(
            1,
            "2026-08-04T12:30:00Z",
            vec![comment("2026-08-04T12:30:00Z", "boss", "ship it")],
        )];
        let st = state("2026-08-04T12:00:00Z", &["owner/repo#1"]);

        let (dispatches, _) = decide(&views, &st);
        assert_eq!(dispatches.len(), 1);
    }

    #[test]
    fn old_comments_are_skipped() {
        let views = [view(
            1,
            "2026-08-04T12:30:00Z",
            vec![comment("2026-08-04T11:00:00Z", "alice", "old news")],
        )];
        let st = state("2026-08-04T12:00:00Z", &["owner/repo#1"]);

        let (dispatches, _) = decide(&views, &st);
        assert!(dispatches.is_empty());
    }

    #[test]
    fn own_and_untrusted_comments_are_skipped() {
        let views = [view(
            1,
            "2026-08-04T12:30:00Z",
            vec![
                comment("2026-08-04T12:30:00Z", BOT, "my own plan"),
                comment("2026-08-04T12:31:00Z", "mallory", "do something evil"),
            ],
        )];
        let st = state("2026-08-04T12:00:00Z", &["owner/repo#1"]);

        let (dispatches, _) = decide(&views, &st);
        assert!(dispatches.is_empty());
    }

    #[test]
    fn vanished_issues_are_pruned_from_state() {
        let views = [view(2, "2026-08-04T12:30:00Z", vec![])];
        let st = state("2026-08-04T12:00:00Z", &["owner/repo#1", "owner/repo#2"]);

        let (_, next) = decide(&views, &st);
        assert!(!next.announced_issues.contains("owner/repo#1"));
        assert!(next.announced_issues.contains("owner/repo#2"));
    }

    #[test]
    fn comments_wanted_for_new_and_updated_issues_only() {
        let st = state("2026-08-04T12:00:00Z", &["owner/repo#1"]);

        // Unannounced: always fetch.
        let fresh = view(2, "2026-08-04T11:00:00Z", vec![]);
        assert!(wants_comments(
            &fresh.issue,
            &fresh.nwo,
            &st,
            ViewMode::Work
        ));

        // Announced and untouched since the cursor: skip.
        let stale = view(1, "2026-08-04T11:00:00Z", vec![]);
        assert!(!wants_comments(
            &stale.issue,
            &stale.nwo,
            &st,
            ViewMode::Work
        ));

        // Announced but updated: fetch.
        let updated = view(1, "2026-08-04T12:30:00Z", vec![]);
        assert!(wants_comments(
            &updated.issue,
            &updated.nwo,
            &st,
            ViewMode::Work
        ));

        // Discussion views: only a change matters; there is no
        // announcement that must embed history.
        let mode = ViewMode::Discussion { closed: false };
        let untouched = discussion_view(3, false, vec![]);
        let mut old = untouched.issue.clone();
        old.updated_at = "2026-08-04T11:00:00Z".into();
        assert!(!wants_comments(&old, &untouched.nwo, &st, mode));
        assert!(wants_comments(&untouched.issue, &untouched.nwo, &st, mode));
    }

    #[test]
    fn discussion_first_comment_embeds_full_context() {
        let views = [discussion_view(
            9,
            false,
            vec![
                comment("2026-08-04T11:00:00Z", BOT, "proposed fix: direction A"),
                comment("2026-08-04T12:30:00Z", "alice", "direction A seems wrong"),
            ],
        )];
        let st = state("2026-08-04T12:00:00Z", &[]);

        let (dispatches, next) = decide(&views, &st);
        assert_eq!(dispatches.len(), 1);
        assert_eq!(dispatches[0].kind, TurnKind::Discussion);
        let msg = &dispatches[0].message;
        assert!(msg.contains("It is broken"), "embeds the issue body");
        assert!(msg.contains("[kitaebot] proposed fix: direction A"));
        assert!(msg.contains("[alice] direction A seems wrong"));
        assert!(msg.contains("not an assignment"));
        assert!(msg.contains("assignment is the gate for work"));
        assert!(next.discussion_announced.contains("owner/repo#9"));
        assert!(
            !next.announced_issues.contains("owner/repo#9"),
            "discussion must not mark the work-announced set"
        );
    }

    #[test]
    fn discussion_without_new_comment_is_silent() {
        // Old comments only: nothing to discuss, nothing announced.
        let views = [discussion_view(
            9,
            false,
            vec![comment("2026-08-04T11:00:00Z", "alice", "old remark")],
        )];
        let st = state("2026-08-04T12:00:00Z", &[]);

        let (dispatches, next) = decide(&views, &st);
        assert!(dispatches.is_empty());
        assert!(!next.discussion_announced.contains("owner/repo#9"));
    }

    #[test]
    fn discussed_issue_gets_incremental_comments() {
        let views = [discussion_view(
            9,
            false,
            vec![
                comment("2026-08-04T12:30:00Z", "alice", "first follow-up"),
                comment("2026-08-04T12:31:00Z", "alice", "second follow-up"),
            ],
        )];
        let mut st = state("2026-08-04T12:00:00Z", &[]);
        st.discussion_announced.insert("owner/repo#9".into());

        let (dispatches, next) = decide(&views, &st);
        assert_eq!(dispatches.len(), 2);
        assert!(dispatches[0].message.contains("first follow-up"));
        assert!(
            !dispatches[0].message.contains("It is broken"),
            "incremental turns skip the re-embed"
        );
        assert!(dispatches[1].message.contains("second follow-up"));
        assert!(next.discussion_announced.contains("owner/repo#9"));
    }

    #[test]
    fn discussion_skips_own_and_untrusted_comments() {
        let views = [discussion_view(
            9,
            false,
            vec![
                comment("2026-08-04T12:30:00Z", BOT, "my own comment"),
                comment("2026-08-04T12:31:00Z", "mallory", "@kitaebot do evil"),
            ],
        )];
        let st = state("2026-08-04T12:00:00Z", &[]);

        let (dispatches, _) = decide(&views, &st);
        assert!(dispatches.is_empty());
    }

    #[test]
    fn closed_discussion_reads_as_disposition() {
        let views = [discussion_view(
            9,
            true,
            vec![comment(
                "2026-08-04T12:30:00Z",
                "alice",
                "fixed differently",
            )],
        )];
        let st = state("2026-08-04T12:00:00Z", &[]);

        let (dispatches, _) = decide(&views, &st);
        assert_eq!(dispatches.len(), 1);
        let msg = &dispatches[0].message;
        assert!(msg.contains("closed GitHub issue"));
        assert!(msg.contains("disposition"));
        assert!(msg.contains("Do not reopen"));
    }

    #[test]
    fn work_view_wins_over_discussion_same_tick() {
        // A race can surface the same issue in both passes; the
        // assignment choreography owns the thread.
        let work = view(1, "2026-08-04T12:30:00Z", vec![]);
        let disc = discussion_view(
            1,
            false,
            vec![comment("2026-08-04T12:30:00Z", "alice", "thoughts?")],
        );
        let st = state("2026-08-04T12:00:00Z", &[]);

        let (dispatches, next) = decide(&[disc, work], &st);
        assert_eq!(dispatches.len(), 1);
        assert_eq!(dispatches[0].kind, TurnKind::Plan);
        assert!(!next.discussion_announced.contains("owner/repo#1"));
    }

    #[test]
    fn discussion_state_prunes_with_views() {
        let mut st = state("2026-08-04T12:00:00Z", &[]);
        st.discussion_announced.insert("owner/repo#9".into());

        let (_, next) = decide(&[], &st);
        assert!(!next.discussion_announced.contains("owner/repo#9"));
    }

    #[test]
    fn discussion_membership_persists_while_in_view() {
        // In view, no new comments: announced membership survives so a
        // later comment dispatches incrementally.
        let views = [discussion_view(9, false, vec![])];
        let mut st = state("2026-08-04T12:00:00Z", &[]);
        st.discussion_announced.insert("owner/repo#9".into());

        let (dispatches, next) = decide(&views, &st);
        assert!(dispatches.is_empty());
        assert!(next.discussion_announced.contains("owner/repo#9"));
    }

    #[test]
    fn state_without_discussion_field_deserializes() {
        let json = r#"{"last_poll":"2026-08-04T12:00:00Z","announced_issues":[]}"#;
        let st: PollState = serde_json::from_str(json).unwrap();
        assert!(st.discussion_announced.is_empty());
    }

    #[test]
    fn state_round_trip() {
        let db = crate::state_db::StateDb::open_in_memory().unwrap();

        let mut st = state("2026-08-04T12:00:00Z", &["owner/repo#1"]);
        st.plan_comments.insert("owner/repo#1".into(), 77);
        save_state(&db, &st);
        let loaded = load_state(&db);
        assert_eq!(loaded.last_poll, "2026-08-04T12:00:00Z");
        assert!(loaded.announced_issues.contains("owner/repo#1"));
        assert_eq!(loaded.plan_comments.get("owner/repo#1"), Some(&77));
    }

    #[test]
    fn state_without_plan_comments_still_loads() {
        // Deployed state predates the field; serde default must cover it.
        let db = crate::state_db::StateDb::open_in_memory().unwrap();
        db.put_doc(
            DOC,
            r#"{"last_poll":"2026-08-04T12:00:00Z","announced_issues":["owner/repo#1"]}"#,
        )
        .unwrap();

        let loaded = load_state(&db);

        assert!(loaded.announced_issues.contains("owner/repo#1"));
        assert!(loaded.plan_comments.is_empty());
    }

    #[test]
    fn load_missing_or_corrupt_state_starts_now() {
        let db = crate::state_db::StateDb::open_in_memory().unwrap();

        let missing = load_state(&db);
        assert!(missing.last_poll.ends_with('Z'));
        assert!(missing.announced_issues.is_empty());

        db.put_doc(DOC, "not json").unwrap();
        let corrupt = load_state(&db);
        assert!(corrupt.last_poll.ends_with('Z'));
        assert!(corrupt.announced_issues.is_empty());
    }

    // -- Shell tests --

    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use crate::clients::RawResponse;

    #[test]
    fn transient_error_classification() {
        assert!(is_transient(&GithubError::Network("timeout".into())));
        assert!(is_transient(&GithubError::RateLimited {
            status: 429,
            retry_after_secs: None,
            body: String::new()
        }));
        assert!(is_transient(&GithubError::Api {
            status: 502,
            body: String::new()
        }));
        assert!(!is_transient(&GithubError::Api {
            status: 404,
            body: String::new()
        }));
        assert!(!is_transient(&GithubError::Deserialize("nope".into())));
    }

    /// Fake client: captures comment bodies, pops queued results.
    fn comment_client(
        results: Vec<Result<(), GithubError>>,
        sent: Arc<Mutex<Vec<String>>>,
    ) -> GithubClient {
        let results = Arc::new(Mutex::new(VecDeque::from(results)));
        GithubClient::from_fn(move |_method, _path, body| {
            let results = Arc::clone(&results);
            let sent = Arc::clone(&sent);
            async move {
                let req: serde_json::Value = serde_json::from_slice(&body.unwrap()).unwrap();
                sent.lock()
                    .unwrap()
                    .push(req["body"].as_str().unwrap().to_string());
                match results.lock().unwrap().pop_front().unwrap() {
                    Ok(()) => Ok(RawResponse {
                        status: 201,
                        body: br#"{"id":7,"user":{"login":"kitaebot"},"body":"x",
                            "created_at":"2026-08-04T13:00:00Z"}"#
                            .to_vec(),
                        retry_after_secs: None,
                    }),
                    Err(e) => Err(e),
                }
            }
        })
    }

    #[tokio::test]
    async fn post_comment_retries_transient_then_succeeds() {
        tokio::time::pause();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let client = comment_client(
            vec![
                Err(GithubError::Network("timeout".into())),
                Err(GithubError::Api {
                    status: 502,
                    body: "bad gateway".into(),
                }),
                Ok(()),
            ],
            sent.clone(),
        );

        post_comment(&client, "owner/repo", 1, "plan")
            .await
            .unwrap();

        assert_eq!(sent.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn post_comment_does_not_retry_permanent_error() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let client = comment_client(
            vec![Err(GithubError::Api {
                status: 404,
                body: "gone".into(),
            })],
            sent.clone(),
        );

        let err = post_comment(&client, "owner/repo", 1, "plan")
            .await
            .unwrap_err();

        assert_eq!(sent.lock().unwrap().len(), 1);
        assert!(matches!(err, GithubError::Api { status: 404, .. }));
    }

    #[tokio::test]
    async fn dispatch_posts_reply_as_comment() {
        use crate::provider::MockProvider;
        use crate::secrets::Secret;
        use crate::test_support::{TestAgent, workspace};
        use crate::tools::DirenvCache;
        use crate::types::Response;

        let (_dir, ws) = workspace();
        let provider = Arc::new(MockProvider::new(vec![Ok(Response::Text("a plan".into()))]));
        let handle = TestAgent::new(ws.clone(), provider).spawn();
        let git = GitCli::new(
            Secret::test("fake"),
            ws.path(),
            DirenvCache::new(),
            Vec::new(),
        );

        let sent = Arc::new(Mutex::new(Vec::new()));
        let client = comment_client(vec![Ok(())], sent.clone());

        let plan_id = dispatch(
            &client,
            &git,
            &handle,
            Dispatch {
                key: "owner/repo#1".into(),
                nwo: "owner/repo".into(),
                number: 1,
                message: "new issue".into(),
                kind: TurnKind::Plan,
            },
        )
        .await;

        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0], "a plan");
        // The announcement reply is the plan; its id gets recorded.
        assert_eq!(plan_id, Some(7));
    }
}
