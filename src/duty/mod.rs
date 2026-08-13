//! The duty scheduler (spec 24, phase 1).
//!
//! A duty is a named unit of scheduled work dispatched through the
//! agent actor as a slash command. Schedules are wall-clock (epoch)
//! based with per-duty `last_run` persisted in `state/duties.json`,
//! so cadence survives restarts, and an overdue duty fires once at
//! startup (anacron catch-up) instead of resetting phase or bursting.

pub mod schedule;
pub mod self_analysis;
pub mod state;

use std::path::PathBuf;
use std::time::Duration;

use crate::state_db::StateDb;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::agent::AgentHandle;
use crate::agent::envelope::ChannelSource;
use crate::clients::github::GithubClient;
use crate::tools::git::GitCli;
use schedule::Schedule;
use state::DutyState;

/// A scheduled duty: performs its [`Action`] when due.
pub struct Duty {
    pub name: String,
    pub action: Action,
    pub schedule: Schedule,
    pub gate: Option<Gate>,
}

/// What a due duty does.
pub enum Action {
    /// Send `input` through the actor: a slash command for built-ins
    /// (`/duty distill`) or the operator's prompt text for prompt
    /// duties; `session_hint` routes onto a repo's work session.
    Dispatch {
        input: String,
        session_hint: Option<String>,
    },
    /// Mine the bot's own problem record and file at most one proposal
    /// on `repo` (spec 24 phase 2).
    SelfAnalysis {
        repo: String,
        min_delta_tokens: usize,
    },
    /// Probe and warm configured repos whose remote HEAD moved, in
    /// the scheduler with no LLM turn (spec 24 self-maintenance).
    Warm,
}

/// An operator run-now request (`/duty <name>`, `/duties`): run one
/// duty or all of them immediately, gates respected, schedules not.
pub struct Trigger {
    /// A single duty, or `None` for every duty.
    pub name: Option<String>,
}

/// The actor's end of the trigger channel: the duty names for local
/// validation and the sender the scheduler listens on.
#[derive(Clone)]
pub struct TriggerHandle {
    pub names: Vec<String>,
    pub tx: mpsc::Sender<Trigger>,
}

/// A mechanical pre-dispatch check: decides whether a due duty gets a
/// turn at all, at the cost of zero tokens.
pub enum Gate {
    /// Open only when the repo's remote head moved past the cursor.
    NewCommits { repo: String },
}

/// What a new-commits probe decides, given cursor and remote head.
#[derive(Debug, PartialEq, Eq)]
enum NewCommits {
    /// No cursor yet: record the head, dispatch nothing. First contact
    /// must not review the repo's entire history.
    Prime,
    /// Head unchanged: nothing to review.
    Closed,
    /// Head moved: dispatch, and advance the cursor on success.
    Open,
}

fn new_commits_decision(cursor: Option<&str>, head: &str) -> NewCommits {
    match cursor {
        None => NewCommits::Prime,
        Some(c) if c == head => NewCommits::Closed,
        Some(_) => NewCommits::Open,
    }
}

/// Journal a mechanical duty's outcome. Dispatch duties are journaled
/// by the actor like every unattended turn; mechanical duties never
/// pass through it, so the scheduler writes their record itself.
fn record(journal_path: &std::path::Path, name: &str, outcome: &str) {
    if let Err(e) = crate::workspace::journal(journal_path, "duty", &format!("{name}: {outcome}")) {
        warn!(duty = %name, "failed to journal duty outcome: {e}");
    }
}

/// Cap on scheduler sleep. `tokio::time::sleep` is monotonic and
/// diverges from the wall clock across host pauses; re-checking
/// bounds the divergence without ever dispatching early.
const MAX_SLEEP_SECS: u64 = 600;

/// Duties a trigger names: all of them, or one by name. The unknown
/// name is a caller bug — the actor validates against the same list.
fn resolve<'a>(duties: &'a [Duty], name: Option<&str>) -> Result<Vec<&'a Duty>, String> {
    match name {
        None => Ok(duties.iter().collect()),
        Some(n) => duties
            .iter()
            .find(|d| d.name == n)
            .map(|d| vec![d])
            .ok_or_else(|| {
                let known: Vec<&str> = duties.iter().map(|d| d.name.as_str()).collect();
                format!("unknown duty {n:?}; known: {}", known.join(", "))
            }),
    }
}

/// Duties due at `now`, in declaration order.
fn due<'a>(duties: &'a [Duty], state: &DutyState, now: u64) -> Vec<&'a Duty> {
    duties
        .iter()
        .filter(|d| d.schedule.next_due(state.last_run(&d.name), now) <= now)
        .collect()
}

/// Seconds until the earliest next due time, capped at
/// [`MAX_SLEEP_SECS`].
fn next_wake(duties: &[Duty], state: &DutyState, now: u64) -> u64 {
    duties
        .iter()
        .map(|d| {
            d.schedule
                .next_due(state.last_run(&d.name), now)
                .saturating_sub(now)
        })
        .min()
        .unwrap_or(MAX_SLEEP_SECS)
        .clamp(1, MAX_SLEEP_SECS)
}

/// Run the duty scheduler loop.
///
/// Serialization comes free: `send_message` awaits the actor, so two
/// duties due together run in sequence, in declaration order.
#[allow(clippy::too_many_arguments)]
pub async fn run_loop(
    duties: Vec<Duty>,
    state_db: StateDb,
    journal_path: PathBuf,
    errors_dir: PathBuf,
    handle: &AgentHandle,
    git: Option<GitCli>,
    github: Option<GithubClient>,
    mut triggers: mpsc::Receiver<Trigger>,
) -> ! {
    let ctx = RunCtx {
        sources: self_analysis::Sources {
            journal: journal_path.clone(),
            errors_dir,
        },
        state_db,
        journal_path,
        handle,
        git,
        github,
    };
    let mut state = DutyState::load(&ctx.state_db);
    loop {
        let now = crate::time::now_epoch();
        for duty in due(&duties, &state, now) {
            run_one(duty, &ctx, &mut state).await;
        }
        let now = crate::time::now_epoch();
        let wake = Duration::from_secs(next_wake(&duties, &state, now));
        tokio::select! {
            () = tokio::time::sleep(wake) => {}
            Some(trigger) = triggers.recv() => {
                match resolve(&duties, trigger.name.as_deref()) {
                    Ok(named) => {
                        for duty in named {
                            info!(duty = %duty.name, "Duty triggered by operator");
                            run_one(duty, &ctx, &mut state).await;
                        }
                    }
                    Err(e) => warn!("duty trigger: {e}"),
                }
            }
        }
    }
}

/// Everything a duty run borrows besides the mutable state.
struct RunCtx<'a> {
    sources: self_analysis::Sources,
    state_db: StateDb,
    journal_path: PathBuf,
    handle: &'a AgentHandle,
    git: Option<GitCli>,
    github: Option<GithubClient>,
}

/// One duty run — the shared execution path for scheduled and
/// triggered runs alike: same gates, same journaling, and `last_run`
/// advances either way, so a manual run defers the next tick.
async fn run_one(duty: &Duty, ctx: &RunCtx<'_>, state: &mut DutyState) {
    match &duty.action {
        Action::Dispatch {
            input,
            session_hint,
        } => {
            dispatch(
                duty,
                input,
                session_hint.clone(),
                ctx.handle,
                ctx.git.as_ref(),
                state,
            )
            .await;
        }
        Action::SelfAnalysis {
            repo,
            min_delta_tokens,
        } => {
            run_self_analysis(
                duty,
                repo,
                *min_delta_tokens,
                &ctx.sources,
                ctx.handle,
                ctx.github.as_ref(),
                &ctx.journal_path,
                state,
            )
            .await;
        }
        Action::Warm => {
            let outcome = match ctx.git.as_ref() {
                Some(git) => run_warm(git, state).await,
                None => "skipped: no GitCli".into(),
            };
            info!(duty = %duty.name, "Duty run: {outcome}");
            record(&ctx.journal_path, &duty.name, &outcome);
        }
    }
    // last_run advances even on error or closed gate: retry
    // next period, not in a tight loop (spec 24 failure modes).
    state.record_run(&duty.name, crate::time::now_epoch());
    state.save(&ctx.state_db);
}

/// Gate-check and send one dispatch duty through the actor. The
/// outcome is journaled by the actor (unattended turn), not here.
async fn dispatch(
    duty: &Duty,
    input: &str,
    session_hint: Option<String>,
    handle: &AgentHandle,
    git: Option<&GitCli>,
    state: &mut DutyState,
) {
    // (input, cursor to advance on success)
    let dispatch: Option<(String, Option<String>)> = match &duty.gate {
        None => Some((input.to_string(), None)),
        Some(Gate::NewCommits { repo }) => probe_new_commits(duty, input, repo, git, state).await,
    };
    let Some((input, new_cursor)) = dispatch else {
        return;
    };
    let cancel = CancellationToken::new();
    match handle
        .send_message(ChannelSource::Duty, input, session_hint, None, cancel)
        .await
    {
        Ok(reply) => {
            info!(duty = %duty.name, "Duty run: {}", reply.content);
            // Advance only on success: a failed turn re-reviews the
            // same delta next period.
            if let Some(head) = new_cursor {
                state.set_cursor(&duty.name, &head);
            }
        }
        Err(e) => {
            error!(duty = %duty.name, "Duty error (will retry next period): {e}");
        }
    }
}

/// Run one self-analysis pass: probe the symptom sources, enforce the
/// proposal cap, dispatch the analysis turn, advance the cursor on
/// success (a failed turn re-reads the same delta next period).
#[allow(clippy::too_many_arguments)]
async fn run_self_analysis(
    duty: &Duty,
    repo: &str,
    min_delta_tokens: usize,
    sources: &self_analysis::Sources,
    handle: &AgentHandle,
    github: Option<&GithubClient>,
    journal_path: &std::path::Path,
    state: &mut DutyState,
) {
    let Some(client) = github else {
        // Config validation requires github.enabled; reaching here
        // means the invariant broke, not the operator.
        error!(duty = %duty.name, "self-analysis has no GitHub client; skipping");
        return;
    };
    let cursor = state.cursor(&duty.name).and_then(|c| c.parse().ok());
    let probe = match self_analysis::probe(sources, cursor, min_delta_tokens) {
        Ok(probe) => probe,
        Err(e) => {
            error!(duty = %duty.name, "symptom probe failed (will retry next period): {e}");
            return;
        }
    };
    let (delta, next) = match probe {
        self_analysis::Probe::Prime(cursor) => {
            info!(duty = %duty.name, "self-analysis cursor primed");
            state.set_cursor(&duty.name, &cursor.to_string());
            return;
        }
        self_analysis::Probe::Closed => {
            info!(duty = %duty.name, "self-analysis gate closed");
            return;
        }
        self_analysis::Probe::Open { delta, next } => (delta, next),
    };

    let proposals = match open_proposals(client, repo).await {
        Ok(proposals) => proposals,
        Err(e) => {
            error!(duty = %duty.name, "open-proposal query failed (will retry next period): {e}");
            return;
        }
    };
    if proposals.len() >= self_analysis::PROPOSAL_CAP {
        // The delta stays unconsumed: triage frees the cap, and the
        // next run sees the accumulated material.
        record(
            journal_path,
            &duty.name,
            &format!(
                "skipped: proposal cap reached ({} open on {repo})",
                proposals.len(),
            ),
        );
        return;
    }

    let prompt = self_analysis::format_prompt(repo, &delta, &proposals);
    let cancel = CancellationToken::new();
    match handle
        .send_message(
            ChannelSource::Duty,
            prompt,
            Some(repo.to_string()),
            None,
            cancel,
        )
        .await
    {
        Ok(reply) => {
            info!(duty = %duty.name, "Duty run: {}", reply.content);
            state.set_cursor(&duty.name, &next.to_string());
        }
        Err(e) => {
            error!(duty = %duty.name, "Duty error (will retry next period): {e}");
        }
    }
}

/// Titles of open issues the bot already filed on `repo`, `#N title`
/// formatted for prompt injection.
async fn open_proposals(
    client: &GithubClient,
    repo: &str,
) -> Result<Vec<String>, crate::error::GithubError> {
    let login = client.user().await?.login;
    let issues = client
        .search_issues(&format!("is:issue is:open author:{login} repo:{repo}"))
        .await?;
    Ok(issues
        .iter()
        .map(|i| format!("#{} {}", i.number, i.title))
        .collect())
}

/// Probe the new-commits gate. Returns the dispatch input and the
/// cursor to advance, or `None` when nothing should run (gate closed,
/// first-contact priming, or probe failure).
async fn probe_new_commits(
    duty: &Duty,
    input: &str,
    repo: &str,
    git: Option<&GitCli>,
    state: &mut DutyState,
) -> Option<(String, Option<String>)> {
    let Some(git) = git else {
        // Config validation requires github.enabled for gated duties;
        // reaching here means the invariant broke, not the operator.
        error!(duty = %duty.name, "new-commits gate has no GitCli; skipping");
        return None;
    };
    let head = match git.remote_head(repo).await {
        Ok(head) => head,
        Err(e) => {
            error!(duty = %duty.name, "new-commits probe failed (will retry next period): {e}");
            return None;
        }
    };
    match new_commits_decision(state.cursor(&duty.name), &head) {
        NewCommits::Prime => {
            info!(duty = %duty.name, %head, "new-commits gate primed");
            state.set_cursor(&duty.name, &head);
            None
        }
        NewCommits::Closed => {
            info!(duty = %duty.name, "new-commits gate closed");
            None
        }
        NewCommits::Open => {
            let cursor = state.cursor(&duty.name).expect("Open requires a cursor");
            let input = format!("{input}\n\n[new commits: {cursor}..{head}]");
            Some((input, Some(head)))
        }
    }
}

/// Per-repo cursor key for the warm gate: `warm/<nwo>`.
fn warm_cursor_key(nwo: &str) -> String {
    format!("warm/{nwo}")
}

/// Run the warm duty with per-repo new-commits gating (spec 24).
///
/// Each configured repo is probed via `ls-remote`; only repos whose
/// remote HEAD moved past the cursor (or that have no cursor or no
/// checkout) are warmed. The cursor advances only on a successful
/// warm, so a failed warm retries next tick. Returns a per-repo
/// summary for the duty history log.
async fn run_warm(git: &GitCli, state: &mut DutyState) -> String {
    let repos = git.warm_repos();
    if repos.is_empty() {
        return "no warm commands configured".into();
    }
    let mut lines = Vec::with_capacity(repos.len());
    for nwo in &repos {
        let key = warm_cursor_key(nwo);
        let cursor = state.cursor(&key);
        // Fetch the remote HEAD once: the gate decision and the
        // cursor advance both need it.
        let head = match git.remote_head(nwo).await {
            Ok(h) => h,
            Err(e) => {
                error!(repo = %nwo, "warm gate ls-remote failed: {e}");
                lines.push(format!("{nwo}: skipped (ls-remote failed)"));
                continue;
            }
        };
        // Due when: no cursor (enrollment), no checkout, or HEAD moved.
        let due = cursor.is_none() || !git.checkout_exists(nwo) || cursor != Some(head.as_str());
        if !due {
            lines.push(format!("{nwo}: skipped (no new commits)"));
            continue;
        }
        let status = git.prepare_and_warm(nwo).await;
        if status == "warm" {
            state.set_cursor(&key, &head);
        }
        lines.push(format!("{nwo}: {status}"));
    }
    lines.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn duty(name: &str, schedule: Schedule) -> Duty {
        Duty {
            name: name.to_string(),
            action: Action::Dispatch {
                input: format!("/{name}"),
                session_hint: None,
            },
            schedule,
            gate: None,
        }
    }

    #[test]
    fn resolve_all_one_and_unknown() {
        let duties = duties();
        let all = resolve(&duties, None).unwrap();
        assert_eq!(all.len(), 2);
        let one = resolve(&duties, Some("second")).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].name, "second");
        let Err(err) = resolve(&duties, Some("nope")) else {
            panic!("unknown name must be rejected");
        };
        assert!(err.contains("unknown duty \"nope\""), "{err}");
        assert!(err.contains("first, second"), "{err}");
    }

    #[test]
    fn new_commits_decision_matrix() {
        assert_eq!(new_commits_decision(None, "abc"), NewCommits::Prime);
        assert_eq!(new_commits_decision(Some("abc"), "abc"), NewCommits::Closed);
        assert_eq!(new_commits_decision(Some("abc"), "def"), NewCommits::Open);
    }

    fn duties() -> Vec<Duty> {
        vec![
            duty("first", Schedule::Every(3_600)),
            duty("second", Schedule::Daily(6 * 3_600)),
        ]
    }

    #[test]
    fn due_preserves_declaration_order() {
        let state = DutyState::default();
        // No recorded runs: everything is due, in declaration order.
        let duties = duties();
        let names: Vec<&str> = due(&duties, &state, 1_000)
            .iter()
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(names, ["first", "second"]);
    }

    #[test]
    fn ran_duties_are_not_due() {
        let mut state = DutyState::default();
        let now = 100_000;
        state.record_run("first", now);
        state.record_run("second", now);
        assert!(due(&duties(), &state, now).is_empty());
    }

    #[test]
    fn next_wake_targets_earliest_duty() {
        let mut state = DutyState::default();
        let now = 100_000;
        state.record_run("first", now); // due in 3600
        state.record_run("second", now); // due later
        assert_eq!(next_wake(&duties(), &state, now), 600); // capped
        let short = vec![duty("soon", Schedule::Every(30))];
        state.record_run("soon", now);
        assert_eq!(next_wake(&short, &state, now), 30);
    }

    #[test]
    fn next_wake_never_busy_loops() {
        let state = DutyState::default();
        // Everything due now: still sleeps at least a second.
        assert_eq!(next_wake(&duties(), &state, 1_000), 1);
    }

    // --- warm gate tests ---

    use std::collections::BTreeMap;
    use std::sync::Arc;

    use crate::secrets::Secret;
    use crate::test_support::FakeDirenv;
    use crate::tools::Warmer;

    /// Run a git command in `cwd`, asserting success, returning stdout.
    fn git_cmd(args: &[&str], cwd: &std::path::Path) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} in {} failed: {}",
            args,
            cwd.display(),
            String::from_utf8_lossy(&out.stderr),
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// A bare repo at `<workspace>/o/r.git` with one commit, and an
    /// existing checkout at `<workspace>/projects/o/r` with `.envrc`,
    /// so `ls-remote` returns a real HEAD SHA and `prepare_and_warm`
    /// can provision the devshell. The `GitCli` uses `with_clone_base`
    /// pointing at the workspace root via `file://` so `repo_url("o/r")`
    /// resolves to the bare repo.
    fn warm_test_setup(warm_command: &str) -> (GitCli, std::path::PathBuf, String) {
        let workspace = tempfile::tempdir().unwrap();
        let ws = workspace.path().to_path_buf();

        // Create a non-bare repo with one commit, then clone --bare.
        // This avoids cross-device link errors from pushing to a
        // bare repo that was cloned empty.
        let src = ws.join("o/r-src");
        std::fs::create_dir_all(&src).unwrap();
        git_cmd(&["init", src.to_str().unwrap()], &ws);
        for (k, v) in [
            ("user.email", "t@t"),
            ("user.name", "t"),
            ("commit.gpgsign", "false"),
        ] {
            git_cmd(&["config", k, v], &src);
        }
        std::fs::write(src.join("README"), "init").unwrap();
        std::fs::write(src.join(".envrc"), "use flake").unwrap();
        git_cmd(&["add", "."], &src);
        git_cmd(&["commit", "-m", "init"], &src);

        // Clone to bare — same filesystem, no cross-device issue.
        let bare = ws.join("o/r.git");
        git_cmd(
            &[
                "clone",
                "--bare",
                src.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
            &ws,
        );

        // Create the checkout that `prepare_and_warm` expects.
        let checkout = ws.join("projects/o/r");
        git_cmd(
            &["clone", bare.to_str().unwrap(), checkout.to_str().unwrap()],
            &ws,
        );
        // Set origin to a GitHub URL so `origin_nwo` resolves.
        git_cmd(
            &["remote", "set-url", "origin", "https://github.com/o/r.git"],
            &checkout,
        );

        // Get the HEAD sha from the bare repo.
        let head = git_cmd(&["rev-parse", "HEAD"], &bare);
        let head = head.trim().to_string();

        // Leak the workspace and direnv tempdir so they outlive this
        // helper (same pattern as workspace_with_checkout in git_cli).
        let ws = workspace.keep();
        let direnv = FakeDirenv::install("echo '{}'");
        let direnv_cache = direnv.cache();
        std::mem::forget(direnv);
        let commands: BTreeMap<String, String> =
            [("o/r".to_string(), warm_command.to_string())].into();
        let git = GitCli::new(
            Secret::test("fake"),
            ws.clone(),
            direnv_cache.clone(),
            vec!["o/r".into()],
        )
        .with_clone_base(&format!("file://{}", ws.to_str().unwrap()))
        .with_warm(Warmer::new(direnv_cache), Arc::new(commands));

        (git, ws, head)
    }

    #[tokio::test]
    async fn warm_gate_skips_when_cursor_matches_head() {
        let (git, _ws, head) = warm_test_setup("touch .warmed");
        let mut state = DutyState::default();
        state.set_cursor(&warm_cursor_key("o/r"), &head);

        let summary = run_warm(&git, &mut state).await;

        assert!(
            summary.contains("skipped (no new commits)"),
            "unchanged cursor must skip: {summary}"
        );
        assert_eq!(state.cursor(&warm_cursor_key("o/r")), Some(head.as_str()));
    }

    #[tokio::test]
    async fn warm_gate_warms_when_head_moved() {
        let (git, ws, head) = warm_test_setup("touch .warmed");
        let mut state = DutyState::default();
        state.set_cursor(&warm_cursor_key("o/r"), "stale-sha");

        let summary = run_warm(&git, &mut state).await;

        assert!(
            summary.contains("o/r: warm"),
            "moved HEAD must warm: {summary}"
        );
        assert_eq!(state.cursor(&warm_cursor_key("o/r")), Some(head.as_str()));
        let warmed = ws.join("projects/o/r/.warmed");
        assert!(warmed.exists(), "warm command must have run");
    }

    #[tokio::test]
    async fn warm_gate_warms_enrollment_no_cursor() {
        let (git, ws, head) = warm_test_setup("touch .warmed");
        let mut state = DutyState::default();

        let summary = run_warm(&git, &mut state).await;

        assert!(
            summary.contains("o/r: warm"),
            "no cursor must warm (enrollment): {summary}"
        );
        assert_eq!(state.cursor(&warm_cursor_key("o/r")), Some(head.as_str()));
        let warmed = ws.join("projects/o/r/.warmed");
        assert!(warmed.exists(), "warm command must have run");
    }

    #[tokio::test]
    async fn warm_gate_warms_when_checkout_missing() {
        let (git, ws, head) = warm_test_setup("touch .warmed");
        let mut state = DutyState::default();
        state.set_cursor(&warm_cursor_key("o/r"), &head);

        // Remove the checkout so the gate sees it as missing.
        std::fs::remove_dir_all(ws.join("projects/o/r")).unwrap();

        // The gate sees no checkout and marks the repo due. The warm
        // clones from the file:// bare repo, but origin_nwo won't
        // resolve for a file:// clone, so prepare_and_warm returns a
        // skip status — the gate decision is what we test here.
        let summary = run_warm(&git, &mut state).await;

        assert!(
            !summary.contains("skipped (no new commits)"),
            "missing checkout must not be skipped: {summary}"
        );
        assert_eq!(state.cursor(&warm_cursor_key("o/r")), Some(head.as_str()));
    }

    #[tokio::test]
    async fn warm_gate_does_not_advance_cursor_on_failure() {
        let (git, _ws, _head) = warm_test_setup("exit 1");
        let mut state = DutyState::default();

        let summary = run_warm(&git, &mut state).await;

        assert!(
            summary.contains("o/r: failed"),
            "failing command must report failed: {summary}"
        );
        assert_eq!(state.cursor(&warm_cursor_key("o/r")), None);
    }

    #[tokio::test]
    async fn warm_gate_empty_repos() {
        let workspace = tempfile::tempdir().unwrap();
        let direnv = FakeDirenv::install("echo '{}'").cache();
        let git = GitCli::new(
            Secret::test("fake"),
            workspace.path().to_path_buf(),
            direnv,
            vec![],
        );
        let mut state = DutyState::default();

        let summary = run_warm(&git, &mut state).await;

        assert_eq!(summary, "no warm commands configured");
    }
}
