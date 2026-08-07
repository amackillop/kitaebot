//! GitHub issue polling channel.
//!
//! Polls for open issues assigned to the bot account in the configured
//! repositories. New issues are announced to the agent, which replies
//! with an implementation plan; comments from trusted users drive plan
//! revision or end-to-end execution. Replies are posted back as issue
//! comments. Assignment is the human gate: an issue nobody assigned to
//! the bot — its own included — dispatches nothing.
//!
//! This module holds the pure core: event detection, message
//! formatting, and poll-state persistence. The poll loop is the thin
//! effectful shell on top.

use std::collections::BTreeSet;
use std::fmt::Write;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::{self, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::execution_checkout;
use super::github::Trust;
use crate::agent::AgentHandle;
use crate::agent::envelope::ChannelSource;
use crate::clients::github::{GithubClient, IssueComment, SearchIssue};
use crate::config::GithubConfig;
use crate::error::GithubError;
use crate::state_db::StateDb;
use crate::time::now_iso8601;
use crate::tools::git::GitCli;

/// Maximum retries for posting a reply comment on transient failures.
const POST_RETRIES: u32 = 3;

/// Whether a [`GithubError`] is worth retrying.
fn is_transient(err: &GithubError) -> bool {
    match err {
        GithubError::Api { status, .. } => *status == 429 || (500..=599).contains(status),
        GithubError::Deserialize(_) => false,
        GithubError::Network(_) => true,
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
            &now_iso8601(),
        );
        let count = dispatches.len();
        for d in dispatches {
            dispatch(client, git, handle, d).await;
        }
        info!(count, "GitHub issues poll: dispatched {count} items");

        state = next;
        save_state(state_db, &state);
    }
}

/// Fetch assigned open issues and the comments of the ones that need
/// them: new issues embed their history in the announcement, updated
/// ones are scanned for new comments, untouched ones skip the fetch.
async fn fetch_views(
    client: &GithubClient,
    bot_login: &str,
    repos: &[String],
    state: &PollState,
) -> Result<Vec<IssueView>, GithubError> {
    let issues = client
        .search_issues(&format!("is:issue is:open assignee:{bot_login}"))
        .await?;

    let mut views = Vec::new();
    for issue in issues {
        let Some(nwo) = issue.nwo() else {
            warn!(
                number = issue.number,
                "Skipping issue with unparseable repository URL"
            );
            continue;
        };
        if !repos.contains(&nwo) {
            warn!(
                issue = %format!("{nwo}#{}", issue.number),
                "Skipping issue in unconfigured repository"
            );
            continue;
        }
        let comments = if wants_comments(&issue, &nwo, state) {
            client.issue_comments(&nwo, issue.number).await?
        } else {
            Vec::new()
        };
        views.push(IssueView {
            issue,
            nwo,
            comments,
        });
    }
    Ok(views)
}

/// Prepare a fresh base checkout for an execution turn and describe it
/// for the agent, or `None` when the turn needs no checkout.
async fn checkout_note(git: &GitCli, d: &Dispatch) -> Option<String> {
    if !d.needs_checkout {
        return None;
    }
    match execution_checkout::prepare(git, &d.nwo).await {
        Ok(rel) => Some(execution_checkout::ready_note(&rel)),
        Err(e) => {
            warn!(issue = %d.key, "execution checkout prep failed: {e}");
            Some(execution_checkout::CLONE_YOURSELF.into())
        }
    }
}

/// Run one agent turn and post the reply (or error) as a comment.
async fn dispatch(client: &GithubClient, git: &GitCli, handle: &AgentHandle, d: Dispatch) {
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
        .send_message(source, message, Some(d.nwo.clone()), None, cancel)
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
    if let Err(e) = post_comment(client, &d.nwo, d.number, &body).await {
        error!("GitHub issue {}: failed to post comment: {e}", d.key);
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
) -> Result<(), GithubError> {
    let mut attempts = 0u32;
    loop {
        match client.create_issue_comment(nwo, number, body).await {
            Ok(_) => return Ok(()),
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

/// An assigned issue with its routing key and (possibly skipped)
/// comment fetch resolved.
pub struct IssueView {
    pub issue: SearchIssue,
    /// `owner/repo`, parsed from the repository URL.
    pub nwo: String,
    pub comments: Vec<IssueComment>,
}

impl IssueView {
    /// Tracking key, `owner/repo#42`.
    fn key(&self) -> String {
        format!("{}#{}", self.nwo, self.issue.number)
    }
}

/// Whether an issue's comments must be fetched this tick: always for
/// unannounced issues (the announcement embeds them), otherwise only
/// when the issue changed since the cursor.
pub fn wants_comments(issue: &SearchIssue, nwo: &str, state: &PollState) -> bool {
    !state
        .announced_issues
        .contains(&format!("{nwo}#{}", issue.number))
        || issue.updated_at.as_str() > state.last_poll.as_str()
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
    /// Whether a fresh base checkout is prepared before the turn.
    /// True for comment turns (which may execute), false for the
    /// plan-only new-issue announcement.
    pub needs_checkout: bool,
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
    now: &str,
) -> (Vec<Dispatch>, PollState) {
    let mut dispatches = Vec::new();
    let mut announced = BTreeSet::new();

    for view in views {
        let key = view.key();
        if !state.announced_issues.contains(&key) {
            dispatches.push(Dispatch {
                key: key.clone(),
                nwo: view.nwo.clone(),
                number: view.issue.number,
                message: format_new_issue(view),
                needs_checkout: false,
            });
            announced.insert(key);
            continue;
        }
        announced.insert(key.clone());

        for comment in &view.comments {
            if comment.created_at.as_str() <= state.last_poll.as_str() {
                continue;
            }
            if comment.user.login == bot_login {
                continue;
            }
            if !trust.allows(&comment.user.login) {
                warn!(
                    issue = %key,
                    author = %comment.user.login,
                    "Skipping comment from untrusted user"
                );
                continue;
            }
            dispatches.push(Dispatch {
                key: key.clone(),
                nwo: view.nwo.clone(),
                number: view.issue.number,
                message: format_comment(view, &comment.user.login, &comment.body),
                needs_checkout: true,
            });
        }
    }

    let next = PollState {
        last_poll: now.to_string(),
        // Keys absent from the fetch (closed, unassigned) are pruned
        // by rebuilding from fetched issues only.
        announced_issues: announced,
    };
    (dispatches, next)
}

// ---------------------------------------------------------------------------
// Message formatting
// ---------------------------------------------------------------------------

fn format_new_issue(view: &IssueView) -> String {
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
    if !view.comments.is_empty() {
        let _ = writeln!(s, "\nExisting comments:");
        for comment in &view.comments {
            let _ = writeln!(s, "[{}] {}", comment.user.login, comment.body);
        }
    }
    let _ = writeln!(s, "\n{}", super::PLAN_INSTRUCTIONS);
    s
}

fn format_comment(view: &IssueView, author: &str, body: &str) -> String {
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

    fn view(number: u32, updated_at: &str, comments: Vec<IssueComment>) -> IssueView {
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
            },
            nwo: "owner/repo".into(),
            comments,
        }
    }

    fn state(last_poll: &str, announced: &[&str]) -> PollState {
        PollState {
            last_poll: last_poll.into(),
            announced_issues: announced.iter().map(|s| (*s).into()).collect(),
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
        decide_events(views, st, BOT, &Trust::new(&config), NOW)
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
        assert!(!dispatches[0].needs_checkout);
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
        assert!(dispatches[0].needs_checkout);
        let msg = &dispatches[0].message;
        assert!(msg.contains("approved, go ahead"));
        assert!(msg.contains("kitaebot_issue-1_<short-summary>"));
        assert!(msg.contains("Closes #1"));
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
        assert!(wants_comments(&fresh.issue, &fresh.nwo, &st));

        // Announced and untouched since the cursor: skip.
        let stale = view(1, "2026-08-04T11:00:00Z", vec![]);
        assert!(!wants_comments(&stale.issue, &stale.nwo, &st));

        // Announced but updated: fetch.
        let updated = view(1, "2026-08-04T12:30:00Z", vec![]);
        assert!(wants_comments(&updated.issue, &updated.nwo, &st));
    }

    #[test]
    fn state_round_trip() {
        let db = crate::state_db::StateDb::open_in_memory().unwrap();

        let st = state("2026-08-04T12:00:00Z", &["owner/repo#1"]);
        save_state(&db, &st);
        let loaded = load_state(&db);
        assert_eq!(loaded.last_poll, "2026-08-04T12:00:00Z");
        assert!(loaded.announced_issues.contains("owner/repo#1"));
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
        assert!(is_transient(&GithubError::Api {
            status: 429,
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

        dispatch(
            &client,
            &git,
            &handle,
            Dispatch {
                key: "owner/repo#1".into(),
                nwo: "owner/repo".into(),
                number: 1,
                message: "new issue".into(),
                needs_checkout: false,
            },
        )
        .await;

        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0], "a plan");
    }
}
