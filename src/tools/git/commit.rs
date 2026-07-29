//! `git_commit` tool — commit staged changes with co-author trailers.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tracing::warn;

use super::git_cli::GitCli;
use super::{Tool, ToolCtx};
use crate::activity::{Activity, emit};
use crate::error::ToolError;
use crate::tools::cli_runner::SubprocessCall;
use crate::tools::warm::WarmOutcome;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository directory relative to workspace root.
    repo_dir: String,
    /// Commit message (Co-authored-by trailers are appended automatically).
    message: String,
}

pub struct Commit {
    cli: GitCli,
    co_authors: Vec<String>,
}

impl Tool for Commit {
    fn name(&self) -> &'static str {
        "git_commit"
    }

    fn description(&self) -> &'static str {
        "Commit staged changes with an automatic Co-authored-by trailer"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(Args)).expect("schema serialization failed")
    }

    fn execute(
        &self,
        args: serde_json::Value,
        ctx: ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let args: Args = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
            self.run(&args.repo_dir, &args.message, &ctx).await
        })
    }
}

/// Cadence of `Waiting` activity while a warm holds up the commit.
const WAIT_NOTE_PERIOD: Duration = Duration::from_mins(1);

impl Commit {
    pub fn new(cli: GitCli, co_authors: Vec<String>) -> Self {
        Self { cli, co_authors }
    }

    fn prepare(&self, repo_dir: &str, message: &str) -> Result<SubprocessCall, ToolError> {
        let cwd = self.cli.resolve_repo_dir(repo_dir)?;
        let full_message = format_commit_message(message, &self.co_authors);
        Ok(self.cli.prepare_git(&["commit", "-m", &full_message], &cwd))
    }

    async fn run(&self, repo_dir: &str, message: &str, ctx: &ToolCtx) -> Result<String, ToolError> {
        let cwd = self.cli.resolve_repo_dir(repo_dir)?;
        self.await_warm(&cwd, ctx).await;
        let call = self.prepare(repo_dir, message)?;
        self.cli.exec_git(call, false).await?.format()
    }

    /// Wait out an in-flight build warm before running the hook
    /// (spec 03: only this tool waits on readiness). Absence and
    /// failure proceed — the hook then fails honestly.
    async fn await_warm(&self, cwd: &Path, ctx: &ToolCtx) {
        let wait = self.cli.warmer().ready(cwd);
        tokio::pin!(wait);
        // First tick is delayed so an instant answer emits nothing.
        let start = tokio::time::Instant::now() + WAIT_NOTE_PERIOD;
        let mut ticker = tokio::time::interval_at(start, WAIT_NOTE_PERIOD);
        loop {
            tokio::select! {
                outcome = &mut wait => {
                    if outcome == Some(WarmOutcome::Failed) {
                        warn!(dir = %cwd.display(), "build warm failed; committing anyway");
                    }
                    return;
                }
                _ = ticker.tick() => emit(ctx.activity.as_ref(), Activity::Waiting {
                    tool: "git_commit".into(),
                    on: "build-cache warm".into(),
                }),
            }
        }
    }
}

/// Append `Co-authored-by` trailers to a commit message.
///
/// Returns the message unchanged when `co_authors` is empty. Otherwise
/// appends a blank line followed by one trailer per co-author.
fn format_commit_message(message: &str, co_authors: &[String]) -> String {
    if co_authors.is_empty() {
        return message.to_string();
    }

    let trailer_len: usize = co_authors.iter().map(|a| a.len() + 18).sum();
    let mut msg = String::with_capacity(message.len() + 2 + trailer_len);
    msg.push_str(message);
    msg.push_str("\n\n");
    for author in co_authors {
        msg.push_str("Co-authored-by: ");
        msg.push_str(author);
        msg.push('\n');
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::git::test_helpers::stub_git_cli_with_repo;

    #[test]
    fn format_message_no_co_authors() {
        let msg = format_commit_message("Fix bug", &[]);
        assert_eq!(msg, "Fix bug");
    }

    #[test]
    fn format_message_one_co_author() {
        let authors = ["Alice <alice@example.com>".to_string()];
        let msg = format_commit_message("Fix bug", &authors);
        assert_eq!(
            msg,
            "Fix bug\n\nCo-authored-by: Alice <alice@example.com>\n"
        );
    }

    #[test]
    fn format_message_multiple_co_authors() {
        let authors = [
            "Alice <alice@example.com>".to_string(),
            "Bob <bob@example.com>".to_string(),
        ];
        let msg = format_commit_message("Add feature", &authors);
        assert_eq!(
            msg,
            "Add feature\n\nCo-authored-by: Alice <alice@example.com>\nCo-authored-by: Bob <bob@example.com>\n"
        );
    }

    #[tokio::test]
    async fn await_warm_returns_immediately_when_never_warmed() {
        let (git, repo) = stub_git_cli_with_repo();
        let cwd = git.resolve_repo_dir(&repo).unwrap();
        let tool = Commit::new(git, vec![]);
        // Completes without an in-flight warm to join; a hang here
        // times out the test harness.
        tool.await_warm(&cwd, &ToolCtx::default()).await;
    }

    #[tokio::test]
    async fn await_warm_waits_out_an_in_flight_run() {
        let (git, repo) = stub_git_cli_with_repo();
        let cwd = git.resolve_repo_dir(&repo).unwrap();
        let runner = {
            let (w, p) = (git.warmer().clone(), cwd.clone());
            tokio::spawn(async move { w.warm(&p, "sleep 0.3").await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        let before = std::time::Instant::now();
        Commit::new(git, vec![])
            .await_warm(&cwd, &ToolCtx::default())
            .await;
        assert!(
            before.elapsed() >= Duration::from_millis(100),
            "commit must not run while the warm is in flight"
        );
        runner.await.unwrap();
    }

    #[test]
    fn builds_correct_commit_command() {
        let (git, repo) = stub_git_cli_with_repo();
        let tool = Commit::new(git, vec!["Alice <alice@bob.net>".to_string()]);
        let call = tool.prepare(&repo, "Fix bug").unwrap();
        assert_eq!(call.binary, "git");
        assert_eq!(
            call.args,
            [
                "commit",
                "-m",
                "Fix bug\n\nCo-authored-by: Alice <alice@bob.net>\n"
            ]
        );
        assert!(!call.has_env("GIT_ASKPASS"));
    }
}
