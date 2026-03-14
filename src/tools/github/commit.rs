//! `git_commit` tool — commit staged changes with co-author trailers.

use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;

use super::Tool;
use super::git_cli::GitCli;
use crate::error::ToolError;
use crate::tools::cli_runner::CliRunner;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository directory relative to workspace root.
    repo_dir: String,
    /// Commit message (Co-authored-by trailers are appended automatically).
    message: String,
}

pub struct Commit<R>(pub GitCli<R>);

impl<R: CliRunner> Tool for Commit<R> {
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
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let args: Args = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
            self.run(&args.repo_dir, &args.message).await
        })
    }
}

impl<R: CliRunner> Commit<R> {
    async fn run(&self, repo_dir: &str, message: &str) -> Result<String, ToolError> {
        let cwd = self.0.resolve_repo_dir(repo_dir)?;
        let full_message = format_commit_message(message, self.0.co_authors());
        self.0
            .run_git(&["commit", "-m", &full_message], &cwd, false)
            .await
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
    use crate::tools::github::test_helpers::{ok_output, stub_git_cli_with_repo};

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
    async fn calls_git_commit_unauthenticated() {
        let (git, repo, calls) =
            stub_git_cli_with_repo(vec![ok_output("[master abc1234] Fix bug")]);
        let tool = Commit(git);
        let _ = tool.run(&repo, "Fix bug").await.unwrap();

        let recorded = calls.take().await;
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].binary, "git");
        assert_eq!(recorded[0].args, ["commit", "-m", "Fix bug"]);
        assert!(!recorded[0].has_env("GIT_ASKPASS"));
    }
}
